//! The native run engine's tool loop against scripted provider streams and a
//! test-only tool host. Every identifier, argument and result is generated at
//! runtime.
mod support;

use a206_core::*;
use a206_runtime::{
    gateway::Gateway,
    tools::{ToolHost, ToolListing, ToolOutput, ToolRequest, ToolScope, ToolSpec},
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use support::*;
use tokio::sync::mpsc;

/// Set the usage an Anthropic response reports.
fn with_usage(events: String, input: u64, output: u64) -> String {
    events
        .replace("\"input_tokens\":1", &format!("\"input_tokens\":{input}"))
        .replace(
            "\"output_tokens\":1",
            &format!("\"output_tokens\":{output}"),
        )
}
/// A token count generated at runtime.
fn count() -> u64 {
    (Id::new_v4().as_u128() % 100_000) as u64 + 1
}
/// The arguments a request replays for an earlier `call`: OpenAI's string,
/// or the input object of the other protocols.
fn replayed_arguments(api: ProviderApi, body: &Value, call: &str) -> Option<Value> {
    body["messages"]
        .as_array()?
        .iter()
        .find_map(|message| match api {
            ProviderApi::OpenAi => message["tool_calls"]
                .as_array()?
                .iter()
                .find_map(|c| (c["id"] == call).then(|| c["function"]["arguments"].clone())),
            _ => message["content"].as_array()?.iter().find_map(|block| {
                (block["type"] == "tool_use" && block["id"] == call).then(|| block["input"].clone())
            }),
        })
}

// ── Test-only tool host ─────────────────────────────────────────────────────

/// Tools that echo their arguments with a per-host marker, and one that
/// never finishes until its call is dropped.
struct TestHost {
    tools: Vec<ToolSpec>,
    marker: String,
    blocking: Option<String>,
    calls: mpsc::UnboundedSender<(ToolScope, ToolRequest)>,
    dropped: Arc<AtomicBool>,
    rescoped: Rescoped,
    /// Tools named `held_…` finish only when this is notified.
    release: Arc<tokio::sync::Notify>,
    /// When set, the next listing waits until it is released.
    listing: Arc<Mutex<Option<ListHold>>>,
}
/// Says when a listing has started, then holds it until told to finish.
struct ListHold {
    entered: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}
/// Conversations whose scope `rescope` records, with the capabilities it
/// last saw for each (`None` before any rescope, or once one is gone).
type Rescoped = Arc<Mutex<BTreeMap<Id, Option<Option<BTreeSet<Capability>>>>>>;
/// Sets its flag when the future holding it is dropped.
struct DropFlag(Arc<AtomicBool>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
impl ToolHost for TestHost {
    fn list<'a>(&'a self, _: &'a ToolScope) -> BoxFuture<'a, ToolListing> {
        Box::pin(async move {
            let hold = self.listing.lock().unwrap().take();
            if let Some(hold) = hold {
                hold.entered.send(()).unwrap();
                hold.release.await.unwrap();
            }
            ToolListing {
                tools: self.tools.clone(),
                unavailable: vec![],
            }
        })
    }
    fn call<'a>(
        &'a self,
        scope: &'a ToolScope,
        request: ToolRequest,
    ) -> BoxFuture<'a, Result<ToolOutput, ErrorReport>> {
        Box::pin(async move {
            let blocking = self.blocking.as_deref() == Some(request.name.as_str());
            // Tools named `…lines_…` return their `value` argument as is.
            let text = if request.name.contains("lines_") {
                request.arguments["value"].as_str().unwrap().to_owned()
            } else {
                format!(
                    "{}:{}",
                    self.marker,
                    Value::Object(request.arguments.clone())
                )
            };
            // Tools named `fail_…` report their own failure.
            let is_error = request.name.starts_with("fail_");
            let held = request.name.starts_with("held_");
            let broken = request.name.starts_with("broken_");
            self.calls.send((scope.clone(), request)).unwrap();
            if blocking {
                let _flag = DropFlag(self.dropped.clone());
                std::future::pending::<()>().await;
            }
            if held {
                self.release.notified().await;
            }
            // Tools named `broken_…` fail in the host, quoting the text.
            if broken {
                return Err(ErrorReport::new(ErrorKind::Other, text));
            }
            Ok(ToolOutput { text, is_error })
        })
    }
    fn rescope(&self, scope: &dyn Fn(Id) -> Option<ToolScope>) {
        for (id, seen) in self.rescoped.lock().unwrap().iter_mut() {
            *seen = Some(scope(*id).map(|scope| scope.capabilities));
        }
    }
}
fn tool(name: &str, permission: ToolPermission, capabilities: &[Capability]) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: nonce(),
        parameters: json!({"type": "object", "properties": {}}),
        capabilities: capabilities.iter().copied().collect(),
        permission,
        whole_output: false,
    }
}

// ── Gateway rig ─────────────────────────────────────────────────────────────

struct Rig {
    gateway: Gateway,
    _state: tempfile::TempDir,
    thread: Id,
    model: ModelKey,
    requests: mpsc::Receiver<Opened>,
    calls: mpsc::UnboundedReceiver<(ToolScope, ToolRequest)>,
    marker: String,
    dropped: Arc<AtomicBool>,
    rescoped: Rescoped,
    release: Arc<tokio::sync::Notify>,
    listing: Arc<Mutex<Option<ListHold>>>,
}
async fn rig(api: ProviderApi, tools: Vec<ToolSpec>, blocking: Option<String>) -> Rig {
    let (sender, calls) = mpsc::unbounded_channel();
    let marker = nonce();
    let dropped = Arc::new(AtomicBool::new(false));
    let rescoped = Rescoped::default();
    let release = Arc::new(tokio::sync::Notify::new());
    let listing = Arc::new(Mutex::new(None));
    let state = tempfile::tempdir().unwrap();
    let gateway = Gateway::open(state.path())
        .unwrap()
        .with_tool_host(Arc::new(TestHost {
            tools,
            marker: marker.clone(),
            blocking,
            calls: sender,
            dropped: dropped.clone(),
            rescoped: rescoped.clone(),
            release: release.clone(),
            listing: listing.clone(),
        }));
    let (model, requests) = connect(&gateway, api).await;
    let mut rig = Rig {
        gateway,
        _state: state,
        thread: Id::nil(),
        model,
        requests,
        calls,
        marker,
        dropped,
        rescoped,
        release,
        listing,
    };
    let Reply::Thread(thread) = rig
        .run(Command::CreateThread {
            title: nonce(),
            mode: Mode::Chat,
        })
        .await
    else {
        panic!("thread creation failed");
    };
    rig.thread = thread.id;
    let configured = rig
        .run(Command::ConfigureThread {
            id: thread.id,
            mode: Mode::Chat,
            model: Some(rig.model.clone()),
            subagent_models: [rig.model.clone()].into(),
        })
        .await;
    assert!(matches!(configured, Reply::Thread(_)), "{configured:?}");
    rig
}
impl Rig {
    async fn run(&self, body: Command) -> Reply {
        request(&self.gateway, body).await
    }
    async fn permit(&self, tool: &str, permission: ToolPermission) {
        let reply = self
            .run(Command::SetToolPermission {
                thread: self.thread,
                tool: tool.into(),
                permission: Some(permission),
            })
            .await;
        assert_eq!(reply, Reply::Ok);
    }
    /// Start a run in the background; its reply is the finished thread.
    fn send(&self) -> tokio::task::JoinHandle<Reply> {
        send(&self.gateway, self.thread)
    }
    async fn next_request(&mut self) -> Opened {
        next_request(&mut self.requests).await
    }
    fn thread(&self) -> Thread {
        let state = self.gateway.snapshot().unwrap();
        state
            .threads
            .into_iter()
            .find(|t| t.id == self.thread)
            .unwrap()
    }
    async fn pending_approval(&self) -> ApprovalRequest {
        tokio::time::timeout(PATIENCE, async {
            loop {
                if let Some(request) = self.thread().approvals.first() {
                    return request.clone();
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(wait("an approval request"))
    }
    async fn events(&self) -> Vec<EventKind> {
        let Reply::Events(events) = self.run(Command::Events { after: 0 }).await else {
            panic!("events unavailable");
        };
        events.into_iter().map(|e| e.kind).collect()
    }
    fn executed(&mut self) -> Vec<ToolRequest> {
        std::iter::from_fn(|| self.calls.try_recv().ok().map(|(_, r)| r)).collect()
    }
    async fn resolve(
        &self,
        approval: &ApprovalRequest,
        digest: &str,
        decision: ApprovalDecision,
    ) -> Reply {
        self.run(Command::ResolveApproval {
            thread: self.thread,
            approval: approval.id,
            digest: digest.into(),
            decision,
        })
        .await
    }
}
// ── Tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn allowed_tools_run_and_their_results_return_to_the_model_in_every_protocol() {
    for api in PROTOCOLS {
        let name = nonce();
        let mut rig = rig(api, vec![tool(&name, ToolPermission::Allow, &[])], None).await;
        rig.permit(&name, ToolPermission::Allow).await;
        let task = rig.send();
        let first = rig.next_request().await;
        assert_eq!(offered(api, &first.body), vec![name.clone()], "{api:?}");
        let value = nonce();
        let call = Call::new(&name, json!({"value": value}));
        let signature = nonce();
        first
            .respond(calls_response(api, std::slice::from_ref(&call), &signature))
            .await;

        let second = rig.next_request().await;
        let executed = rig.executed();
        assert_eq!(executed.len(), 1, "{api:?}");
        assert_eq!(executed[0].arguments["value"], value);
        assert!(!executed[0].approved);
        let output = format!("{}:{}", rig.marker, json!({"value": value}));
        assert_eq!(
            result_for(api, &second.body, &call.id),
            Some(output.clone()),
            "{api:?}"
        );
        if api == ProviderApi::Gemini {
            // The provider's replay token goes back with its call.
            let parts = second.body["contents"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|c| c["parts"].as_array().unwrap().clone())
                .collect::<Vec<_>>();
            assert!(
                parts
                    .iter()
                    .any(|p| p["thoughtSignature"] == signature.as_str()
                        && p["functionCall"]["id"] == call.id.as_str())
            );
        }
        let text = nonce();
        second.respond(text_response(api, &text)).await;

        let thread = finished(task).await;
        assert_eq!(
            thread.state,
            RunState::Complete,
            "{api:?}: {:?}",
            thread.error
        );
        let calls = calls_of(&thread);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].state, ToolCallState::Succeeded);
        assert_eq!(calls[0].output.as_deref(), Some(output.as_str()));
        assert_eq!(thread.messages.last().unwrap().text, text);
    }
}

#[tokio::test]
async fn denied_tools_are_not_offered_and_calls_to_them_are_refused() {
    let api = ProviderApi::OpenAi;
    let (unset, ungranted, revoked) = (nonce(), nonce(), nonce());
    let mut rig = rig(
        api,
        vec![
            tool(&unset, ToolPermission::Allow, &[]),
            // Allowed by the user, but the Chat mode never grants Network.
            tool(&ungranted, ToolPermission::Allow, &[Capability::Network]),
            tool(&revoked, ToolPermission::Allow, &[]),
        ],
        None,
    )
    .await;
    rig.permit(&ungranted, ToolPermission::Allow).await;
    rig.permit(&revoked, ToolPermission::Allow).await;
    let Reply::Tools(list) = rig.run(Command::ListTools { thread: rig.thread }).await else {
        panic!("tool listing failed");
    };
    let offer = |name: &str| list.tools.iter().find(|t| t.name == name).unwrap().clone();
    assert_eq!(offer(&unset).effective, ToolPermission::Deny);
    assert_eq!(offer(&unset).permission, None);
    assert_eq!(offer(&ungranted).effective, ToolPermission::Deny);
    assert_eq!(offer(&ungranted).missing, [Capability::Network].into());
    assert_eq!(offer(&revoked).effective, ToolPermission::Allow);

    let task = rig.send();
    let first = rig.next_request().await;
    assert_eq!(offered(api, &first.body), vec![revoked.clone()]);
    // The user revokes the tool while the model is answering.
    assert_eq!(
        rig.run(Command::SetToolPermission {
            thread: rig.thread,
            tool: revoked.clone(),
            permission: None,
        })
        .await,
        Reply::Ok
    );
    let calls = [
        Call::new(&unset, json!({})),
        Call::new(&ungranted, json!({})),
        Call::new(&revoked, json!({})),
        // No tool can answer an approval, whatever the model calls it.
        Call::new("resolve_approval", json!({"decision": "approve"})),
    ];
    first.respond(calls_response(api, &calls, "")).await;
    let second = rig.next_request().await;
    assert!(rig.executed().is_empty(), "a refused call reached the host");
    for call in &calls {
        let result = result_for(api, &second.body, &call.id).unwrap();
        assert!(!result.contains(&rig.marker));
    }
    assert!(
        result_for(api, &second.body, &calls[1].id)
            .unwrap()
            .contains("Network")
    );
    second.respond(text_response(api, &nonce())).await;
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Complete);
    let recorded = calls_of(&thread);
    assert!(recorded.iter().all(|c| c.state == ToolCallState::Denied));
    assert_eq!(error_kind(&recorded[0]), ErrorKind::PermissionDenied);
    assert_eq!(error_kind(&recorded[1]), ErrorKind::PermissionDenied);
    assert_eq!(error_kind(&recorded[2]), ErrorKind::PermissionDenied);
    assert_eq!(error_kind(&recorded[3]), ErrorKind::NotFound);
}

#[tokio::test]
async fn ask_runs_a_call_only_after_the_user_approves_that_exact_call() {
    for api in PROTOCOLS {
        let name = nonce();
        let mut rig = rig(api, vec![tool(&name, ToolPermission::Allow, &[])], None).await;
        rig.permit(&name, ToolPermission::Ask).await;
        let task = rig.send();
        let first = rig.next_request().await;
        assert_eq!(offered(api, &first.body), vec![name.clone()]);
        let value = nonce();
        let call = Call::new(&name, json!({"value": value}));
        first
            .respond(calls_response(api, std::slice::from_ref(&call), &nonce()))
            .await;

        let approval = rig.pending_approval().await;
        assert_eq!(
            (approval.call.as_str(), approval.tool.as_str()),
            (call.id.as_str(), name.as_str())
        );
        assert!(approval.arguments.contains(&value));
        assert!(rig.events().await.contains(&EventKind::ApprovalRequested {
            thread: rig.thread,
            approval: approval.id,
        }));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(rig.executed().is_empty(), "ran before approval");

        // A decision for different arguments, or another conversation, is refused.
        let mismatched = rig
            .resolve(&approval, &nonce(), ApprovalDecision::Approve)
            .await;
        assert!(
            matches!(mismatched, Reply::Error(ref r) if r.kind == ErrorKind::Conflict),
            "{mismatched:?}"
        );
        let elsewhere = rig
            .run(Command::ResolveApproval {
                thread: Id::new_v4(),
                approval: approval.id,
                digest: approval.digest.clone(),
                decision: ApprovalDecision::Approve,
            })
            .await;
        assert!(matches!(elsewhere, Reply::Error(ref r) if r.kind == ErrorKind::NotFound));
        assert_eq!(
            rig.thread().approvals.len(),
            1,
            "a refused decision must not resolve it"
        );

        assert_eq!(
            rig.resolve(&approval, &approval.digest, ApprovalDecision::Approve)
                .await,
            Reply::Ok
        );
        let second = rig.next_request().await;
        let executed = rig.executed();
        assert_eq!(executed.len(), 1);
        assert!(executed[0].approved);
        assert_eq!(executed[0].arguments["value"], value);
        assert!(
            result_for(api, &second.body, &call.id)
                .unwrap()
                .contains(&rig.marker)
        );
        assert!(rig.events().await.contains(&EventKind::ApprovalResolved {
            thread: rig.thread,
            approval: approval.id,
            outcome: ApprovalOutcome::Approved,
        }));
        second.respond(text_response(api, &nonce())).await;
        let thread = finished(task).await;
        assert_eq!(thread.state, RunState::Complete);
        assert!(thread.approvals.is_empty());
        assert_eq!(calls_of(&thread)[0].state, ToolCallState::Succeeded);
    }
}

#[tokio::test]
async fn approvals_are_single_use_and_never_reach_the_model() {
    let api = ProviderApi::Anthropic;
    let name = nonce();
    let mut rig = rig(api, vec![tool(&name, ToolPermission::Allow, &[])], None).await;
    rig.permit(&name, ToolPermission::Ask).await;
    let task = rig.send();
    let arguments = json!({"value": nonce()});
    let first = rig.next_request().await;
    first
        .respond(calls_response(
            api,
            &[Call::new(&name, arguments.clone())],
            "",
        ))
        .await;
    let approval = rig.pending_approval().await;
    assert_eq!(
        rig.resolve(&approval, &approval.digest, ApprovalDecision::Approve)
            .await,
        Reply::Ok
    );
    // Answering again, even with the right digest, finds nothing to approve.
    let replay = rig
        .resolve(&approval, &approval.digest, ApprovalDecision::Approve)
        .await;
    assert!(matches!(replay, Reply::Error(ref r) if r.kind == ErrorKind::NotFound));

    // The model repeats the identical call: it needs a fresh approval.
    let second = rig.next_request().await;
    assert_eq!(rig.executed().len(), 1);
    let after_first = second.body.clone();
    second
        .respond(calls_response(api, &[Call::new(&name, arguments)], ""))
        .await;
    let again = rig.pending_approval().await;
    assert_ne!(again.id, approval.id);
    assert_eq!(again.digest, approval.digest);
    let stale = rig
        .resolve(&approval, &approval.digest, ApprovalDecision::Approve)
        .await;
    assert!(matches!(stale, Reply::Error(ref r) if r.kind == ErrorKind::NotFound));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(rig.executed().is_empty(), "an old approval ran a new call");
    assert_eq!(
        rig.resolve(&again, &again.digest, ApprovalDecision::Approve)
            .await,
        Reply::Ok
    );
    let third = rig.next_request().await;
    // Nothing the model receives identifies an approval it could answer.
    for body in [&after_first, &third.body] {
        let text = body.to_string();
        assert!(!text.contains(&approval.id.to_string()) && !text.contains(&again.id.to_string()));
        assert!(!text.contains(&approval.digest));
    }
    third.respond(text_response(api, &nonce())).await;
    assert_eq!(finished(task).await.state, RunState::Complete);
}

#[tokio::test]
async fn a_denied_approval_returns_a_refusal_to_the_model() {
    let api = ProviderApi::Gemini;
    let name = nonce();
    let mut rig = rig(api, vec![tool(&name, ToolPermission::Allow, &[])], None).await;
    rig.permit(&name, ToolPermission::Ask).await;
    let task = rig.send();
    let first = rig.next_request().await;
    let call = Call::new(&name, json!({"value": nonce()}));
    first
        .respond(calls_response(api, std::slice::from_ref(&call), &nonce()))
        .await;
    let approval = rig.pending_approval().await;
    assert_eq!(
        rig.resolve(&approval, &approval.digest, ApprovalDecision::Deny)
            .await,
        Reply::Ok
    );
    let second = rig.next_request().await;
    assert!(rig.executed().is_empty());
    let result = result_for(api, &second.body, &call.id).unwrap();
    assert!(result.contains("denied"), "{result}");
    assert!(rig.events().await.contains(&EventKind::ApprovalResolved {
        thread: rig.thread,
        approval: approval.id,
        outcome: ApprovalOutcome::Denied,
    }));
    second.respond(text_response(api, &nonce())).await;
    let thread = finished(task).await;
    let recorded = &calls_of(&thread)[0];
    assert_eq!(recorded.state, ToolCallState::Denied);
    assert_eq!(error_kind(recorded), ErrorKind::PermissionDenied);
}

#[tokio::test]
async fn budgets_stop_a_run_that_keeps_calling_tools() {
    let api = ProviderApi::OpenAi;
    let name = nonce();
    let mut rig = rig(api, vec![tool(&name, ToolPermission::Allow, &[])], None).await;
    rig.permit(&name, ToolPermission::Allow).await;

    // Rounds: the second response's call does not run.
    let budget = ToolBudget {
        tokens: None,
        rounds: 2,
        tool_calls: 8,
    };
    assert_eq!(
        rig.run(Command::SetToolBudget {
            thread: rig.thread,
            budget
        })
        .await,
        Reply::Ok
    );
    let task = rig.send();
    for _ in 0..2 {
        let request = rig.next_request().await;
        request
            .respond(calls_response(
                api,
                &[Call::new(&name, json!({"value": nonce()}))],
                "",
            ))
            .await;
    }
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Failed);
    let report = thread.error.clone().unwrap();
    assert_eq!(report.kind, ErrorKind::PermissionDenied);
    assert!(
        report.message.contains("2 model rounds"),
        "{}",
        report.message
    );
    assert_eq!(rig.executed().len(), 1);
    let last = calls_of(&thread).pop().unwrap();
    assert_eq!(last.state, ToolCallState::Cancelled);

    // Tool calls: of two calls in one response only the first runs.
    let budget = ToolBudget {
        tokens: None,
        rounds: 8,
        tool_calls: 1,
    };
    assert_eq!(
        rig.run(Command::SetToolBudget {
            thread: rig.thread,
            budget
        })
        .await,
        Reply::Ok
    );
    let task = rig.send();
    let request = rig.next_request().await;
    let calls = [
        Call::new(&name, json!({"value": nonce()})),
        Call::new(&name, json!({"value": nonce()})),
    ];
    request.respond(calls_response(api, &calls, "")).await;
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Failed);
    assert!(
        thread
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("1 tool calls")
    );
    assert_eq!(rig.executed().len(), 1);
    let recorded = calls_of(&thread);
    let last_two: Vec<_> = recorded[recorded.len() - 2..]
        .iter()
        .map(|c| c.state)
        .collect();
    assert_eq!(
        last_two,
        vec![ToolCallState::Succeeded, ToolCallState::Cancelled]
    );

    let invalid = rig
        .run(Command::SetToolBudget {
            thread: rig.thread,
            budget: ToolBudget {
                tokens: None,
                rounds: 0,
                tool_calls: 1,
            },
        })
        .await;
    assert!(matches!(invalid, Reply::Error(ref r) if r.kind == ErrorKind::InvalidRequest));
}

#[tokio::test]
async fn cancelling_during_a_tool_call_stops_the_tool() {
    let api = ProviderApi::Anthropic;
    let name = nonce();
    let mut rig = rig(
        api,
        vec![tool(&name, ToolPermission::Allow, &[])],
        Some(name.clone()),
    )
    .await;
    rig.permit(&name, ToolPermission::Allow).await;
    let task = rig.send();
    let first = rig.next_request().await;
    first
        .respond(calls_response(api, &[Call::new(&name, json!({}))], ""))
        .await;
    tokio::time::timeout(PATIENCE, rig.calls.recv())
        .await
        .unwrap_or_else(wait("the tool to start"))
        .unwrap();
    assert_eq!(calls_of(&rig.thread())[0].state, ToolCallState::Running);
    assert!(!rig.dropped.load(Ordering::SeqCst));
    assert_eq!(
        rig.run(Command::CancelRun { thread: rig.thread }).await,
        Reply::Ok
    );
    let thread = finished(task).await;
    assert!(rig.dropped.load(Ordering::SeqCst), "the tool kept running");
    assert_eq!(thread.state, RunState::Cancelled);
    let call = &calls_of(&thread)[0];
    assert_eq!(call.state, ToolCallState::Cancelled);
    assert_eq!(error_kind(call), ErrorKind::Cancelled);
    // No further model round was requested.
    assert!(rig.requests.try_recv().is_err());
}

#[tokio::test]
async fn cancelling_during_an_approval_wait_expires_the_request() {
    let api = ProviderApi::OpenAi;
    let name = nonce();
    let mut rig = rig(api, vec![tool(&name, ToolPermission::Allow, &[])], None).await;
    rig.permit(&name, ToolPermission::Ask).await;
    let task = rig.send();
    let first = rig.next_request().await;
    first
        .respond(calls_response(api, &[Call::new(&name, json!({}))], ""))
        .await;
    let approval = rig.pending_approval().await;
    assert_eq!(
        rig.run(Command::CancelRun { thread: rig.thread }).await,
        Reply::Ok
    );
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Cancelled);
    assert!(thread.approvals.is_empty());
    assert_eq!(calls_of(&thread)[0].state, ToolCallState::Cancelled);
    assert!(rig.events().await.contains(&EventKind::ApprovalResolved {
        thread: rig.thread,
        approval: approval.id,
        outcome: ApprovalOutcome::Expired,
    }));
    let late = rig
        .resolve(&approval, &approval.digest, ApprovalDecision::Approve)
        .await;
    assert!(matches!(late, Reply::Error(ref r) if r.kind == ErrorKind::NotFound));
    assert!(rig.executed().is_empty());
}

#[tokio::test]
async fn malformed_arguments_are_healed_when_safe_and_reported_otherwise() {
    for api in [ProviderApi::OpenAi, ProviderApi::Anthropic] {
        let name = nonce();
        let mut rig = rig(api, vec![tool(&name, ToolPermission::Allow, &[])], None).await;
        rig.permit(&name, ToolPermission::Allow).await;
        let task = rig.send();
        let (value, cut) = (nonce(), nonce());
        let calls = [
            // A fence and a trailing comma: repaired, same meaning.
            Call::raw(&name, &format!("```json\n{{\"value\": \"{value}\",}}\n```")),
            // Cut off mid-string: never completed into a different call.
            Call::raw(&name, &format!("{{\"value\": \"{cut}")),
            // The repaired call again in the same response: runs once.
            Call::raw(&name, &format!("{{\"value\":\"{value}\"}}")),
        ];
        let first = rig.next_request().await;
        first.respond(calls_response(api, &calls, "")).await;
        let second = rig.next_request().await;
        let executed = rig.executed();
        assert_eq!(executed.len(), 1, "{api:?}");
        assert_eq!(executed[0].arguments["value"], value);
        for call in &calls {
            assert!(
                result_for(api, &second.body, &call.id).is_some(),
                "every call gets a result"
            );
        }
        let truncated = result_for(api, &second.body, &calls[1].id).unwrap();
        assert!(truncated.contains("cut off"), "{truncated}");
        // The unusable call is replayed exactly as the model sent it.
        let replayed = replayed_arguments(api, &second.body, &calls[1].id).unwrap();
        let expected = match api {
            ProviderApi::OpenAi => json!(calls[1].arguments),
            _ => json!({ a206_runtime::provider::UNPARSED_ARGUMENTS: calls[1].arguments }),
        };
        assert_eq!(replayed, expected, "{api:?}");
        // The repaired call is replayed as the object it ran with.
        let repaired = replayed_arguments(api, &second.body, &calls[0].id).unwrap();
        let object = match api {
            ProviderApi::OpenAi => serde_json::from_str(repaired.as_str().unwrap()).unwrap(),
            _ => repaired,
        };
        assert_eq!(object, json!({"value": value}), "{api:?}");
        second.respond(text_response(api, &nonce())).await;
        let thread = finished(task).await;
        let recorded = calls_of(&thread);
        assert_eq!(
            recorded.iter().map(|c| c.state).collect::<Vec<_>>(),
            vec![
                ToolCallState::Succeeded,
                ToolCallState::Failed,
                ToolCallState::Skipped
            ]
        );
        assert!(recorded[0].healed && !recorded[2].healed);
        assert_eq!(error_kind(&recorded[1]), ErrorKind::InvalidRequest);
        assert_eq!(recorded[0].digest, recorded[2].digest);
    }
}

#[tokio::test]
async fn delegated_conversations_cannot_permit_more_than_their_parent() {
    let name = nonce();
    let rig = rig(
        ProviderApi::OpenAi,
        vec![tool(&name, ToolPermission::Allow, &[])],
        None,
    )
    .await;
    rig.permit(&name, ToolPermission::Ask).await;
    let Reply::Thread(child) = rig
        .run(Command::CreateSubagent {
            parent: rig.thread,
            title: nonce(),
            mode: Mode::Chat,
            model: rig.model.clone(),
            requested: Default::default(),
            profile: None,
        })
        .await
    else {
        panic!("subagent creation failed");
    };
    let set = |permission| {
        rig.run(Command::SetToolPermission {
            thread: child.id,
            tool: name.clone(),
            permission: Some(permission),
        })
    };
    let wider = set(ToolPermission::Allow).await;
    assert!(matches!(wider, Reply::Error(ref r) if r.kind == ErrorKind::PermissionDenied));
    assert_eq!(set(ToolPermission::Ask).await, Reply::Ok);
    let Reply::Tools(list) = rig.run(Command::ListTools { thread: child.id }).await else {
        panic!("tool listing failed");
    };
    assert_eq!(
        list.tools
            .iter()
            .find(|tool| tool.name == name)
            .unwrap()
            .effective,
        ToolPermission::Ask
    );
    // Narrowing the parent later narrows the child as well.
    assert_eq!(
        rig.run(Command::SetToolPermission {
            thread: rig.thread,
            tool: name.clone(),
            permission: None,
        })
        .await,
        Reply::Ok
    );
    let Reply::Tools(list) = rig.run(Command::ListTools { thread: child.id }).await else {
        panic!("tool listing failed");
    };
    assert_eq!(
        list.tools
            .iter()
            .find(|tool| tool.name == name)
            .unwrap()
            .effective,
        ToolPermission::Deny
    );
}

/// Create a subagent of `parent` permitted to use `tool`, as the rig's thread.
async fn delegate(rig: &mut Rig, mode: Mode, requested: &[Capability], tool: &str) -> Id {
    let parent = rig.thread;
    rig.permit(tool, ToolPermission::Allow).await;
    let Reply::Thread(child) = rig
        .run(Command::CreateSubagent {
            parent,
            title: nonce(),
            mode,
            model: rig.model.clone(),
            requested: requested.iter().copied().collect(),
            profile: None,
        })
        .await
    else {
        panic!("subagent creation failed");
    };
    rig.thread = child.id;
    rig.permit(tool, ToolPermission::Allow).await;
    parent
}
async fn wait_for_call(rig: &mut Rig) -> (ToolScope, ToolRequest) {
    tokio::time::timeout(PATIENCE, rig.calls.recv())
        .await
        .unwrap_or_else(wait("the tool to start"))
        .unwrap()
}

#[tokio::test]
async fn send_stays_pending_while_an_approval_waits_and_returns_the_finished_thread() {
    let api = ProviderApi::OpenAi;
    let name = nonce();
    let mut rig = rig(api, vec![tool(&name, ToolPermission::Allow, &[])], None).await;
    rig.permit(&name, ToolPermission::Ask).await;
    let before = rig.thread().messages.len();
    let task = rig.send();
    let first = rig.next_request().await;
    let value = nonce();
    let call = Call::new(&name, json!({"value": value}));
    first
        .respond(calls_response(api, std::slice::from_ref(&call), ""))
        .await;
    let approval = rig.pending_approval().await;
    // No timing is relied on: a correct Send can never have finished here,
    // and the reply asserted below is the finished thread only if Send
    // waited for the approval and the rest of the run.
    assert!(
        !task.is_finished(),
        "Send replied while its approval waited"
    );
    // The prompt is an approval request, never a chat message.
    let thread = rig.thread();
    assert_eq!(
        thread.messages.len(),
        before + 2,
        "user message and reply only"
    );
    for message in &thread.messages {
        assert!(!message.text.contains(&approval.id.to_string()));
        assert!(!message.text.contains(&approval.digest));
    }
    // Another request, from a trusted client, answers it.
    assert_eq!(
        rig.resolve(&approval, &approval.digest, ApprovalDecision::Approve)
            .await,
        Reply::Ok
    );
    let second = rig.next_request().await;
    assert!(!task.is_finished(), "Send replied before the run finished");
    let text = nonce();
    second.respond(text_response(api, &text)).await;
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Complete, "{:?}", thread.error);
    assert!(thread.approvals.is_empty());
    let recorded = calls_of(&thread);
    assert_eq!(recorded[0].state, ToolCallState::Succeeded);
    let output = format!("{}:{}", rig.marker, json!({"value": value}));
    assert_eq!(recorded[0].output.as_deref(), Some(output.as_str()));
    assert_eq!(thread.messages.last().unwrap().text, text);
}

#[tokio::test]
async fn revoking_a_tool_stops_its_running_call_and_its_approval_wait() {
    let api = ProviderApi::OpenAi;
    let (running, waiting) = (nonce(), nonce());
    let mut rig = rig(
        api,
        vec![
            tool(&running, ToolPermission::Allow, &[]),
            tool(&waiting, ToolPermission::Allow, &[]),
        ],
        Some(running.clone()),
    )
    .await;
    rig.permit(&running, ToolPermission::Allow).await;
    let task = rig.send();
    let first = rig.next_request().await;
    let call = Call::new(&running, json!({}));
    first
        .respond(calls_response(api, std::slice::from_ref(&call), ""))
        .await;
    wait_for_call(&mut rig).await;
    assert!(!rig.dropped.load(Ordering::SeqCst));
    // Narrowing Allow to Ask withdraws authority from a call nobody approved.
    rig.permit(&running, ToolPermission::Ask).await;
    let second = rig.next_request().await;
    assert!(
        rig.dropped.load(Ordering::SeqCst),
        "the revoked call kept running"
    );
    let result = result_for(api, &second.body, &call.id).unwrap();
    assert!(result.contains("withdrawn"), "{result}");
    second.respond(text_response(api, &nonce())).await;
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Complete);
    let recorded = calls_of(&thread);
    assert_eq!(recorded[0].state, ToolCallState::Denied);
    assert_eq!(error_kind(&recorded[0]), ErrorKind::PermissionDenied);

    // An approval wait is withdrawn as soon as the tool is denied.
    rig.permit(&waiting, ToolPermission::Ask).await;
    let task = rig.send();
    let first = rig.next_request().await;
    let call = Call::new(&waiting, json!({}));
    first
        .respond(calls_response(api, std::slice::from_ref(&call), ""))
        .await;
    let approval = rig.pending_approval().await;
    assert_eq!(
        rig.run(Command::SetToolPermission {
            thread: rig.thread,
            tool: waiting.clone(),
            permission: None,
        })
        .await,
        Reply::Ok
    );
    let second = rig.next_request().await;
    assert!(rig.thread().approvals.is_empty());
    assert!(rig.events().await.contains(&EventKind::ApprovalResolved {
        thread: rig.thread,
        approval: approval.id,
        outcome: ApprovalOutcome::Revoked,
    }));
    let late = rig
        .resolve(&approval, &approval.digest, ApprovalDecision::Approve)
        .await;
    assert!(matches!(late, Reply::Error(ref r) if r.kind == ErrorKind::NotFound));
    second.respond(text_response(api, &nonce())).await;
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Complete);
    let revoked = calls_of(&thread).pop().unwrap();
    assert_eq!(revoked.state, ToolCallState::Denied);
    assert_eq!(error_kind(&revoked), ErrorKind::PermissionDenied);
    assert!(rig.executed().is_empty(), "the withdrawn call ran");
}

#[tokio::test]
async fn a_parent_losing_a_grant_stops_its_childs_running_call() {
    let api = ProviderApi::OpenAi;
    let name = nonce();
    let mut rig = rig(
        api,
        vec![tool(
            &name,
            ToolPermission::Allow,
            &[Capability::ReadWorkspace],
        )],
        Some(name.clone()),
    )
    .await;
    let workspace = tempfile::tempdir().unwrap();
    let Reply::Project(project) = rig
        .run(Command::CreateProject {
            name: nonce(),
            root: workspace.path().to_string_lossy().into(),
        })
        .await
    else {
        panic!("project creation failed");
    };
    let manager = project.manager;
    let configured = rig
        .run(Command::ConfigureThread {
            id: manager,
            mode: Mode::Plan,
            model: Some(rig.model.clone()),
            subagent_models: [rig.model.clone()].into(),
        })
        .await;
    assert!(matches!(configured, Reply::Thread(_)), "{configured:?}");
    let grant = |capabilities: &[Capability]| Command::SetBackend {
        thread: manager,
        backend: CodingBackend::Native,
        capabilities: capabilities.iter().copied().collect(),
    };
    assert_eq!(
        rig.run(grant(&[Capability::ReadWorkspace])).await,
        Reply::Ok
    );
    rig.thread = manager;
    delegate(&mut rig, Mode::Plan, &[Capability::ReadWorkspace], &name).await;

    let task = rig.send();
    let first = rig.next_request().await;
    let call = Call::new(&name, json!({}));
    first
        .respond(calls_response(api, std::slice::from_ref(&call), ""))
        .await;
    let (scope, _) = wait_for_call(&mut rig).await;
    assert_eq!(scope.capabilities, [Capability::ReadWorkspace].into());
    // The parent loses the grant while its child's call runs.
    assert_eq!(rig.run(grant(&[])).await, Reply::Ok);
    let second = rig.next_request().await;
    assert!(
        rig.dropped.load(Ordering::SeqCst),
        "the child's call kept running"
    );
    assert!(
        offered(api, &second.body).is_empty(),
        "a denied tool was offered"
    );
    second.respond(text_response(api, &nonce())).await;
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Complete);
    let recorded = calls_of(&thread);
    assert_eq!(recorded[0].state, ToolCallState::Denied);
    assert_eq!(error_kind(&recorded[0]), ErrorKind::PermissionDenied);
    let Reply::Tools(list) = rig.run(Command::ListTools { thread: rig.thread }).await else {
        panic!("tool listing failed");
    };
    let offer = list.tools.iter().find(|tool| tool.name == name).unwrap();
    assert_eq!(offer.effective, ToolPermission::Deny);
    assert_eq!(offer.missing, [Capability::ReadWorkspace].into());
}

#[tokio::test]
async fn changing_a_grant_lets_tool_hosts_stop_work_it_no_longer_covers() {
    let rig = rig(ProviderApi::OpenAi, vec![], None).await;
    let workspace = tempfile::tempdir().unwrap();
    let Reply::Project(project) = rig
        .run(Command::CreateProject {
            name: nonce(),
            root: workspace.path().to_string_lossy().into(),
        })
        .await
    else {
        panic!("project creation failed");
    };
    let manager = project.manager;
    let configured = rig
        .run(Command::ConfigureThread {
            id: manager,
            mode: Mode::Plan,
            model: Some(rig.model.clone()),
            subagent_models: [rig.model.clone()].into(),
        })
        .await;
    assert!(matches!(configured, Reply::Thread(_)), "{configured:?}");
    let gone = Id::new_v4();
    rig.rescoped
        .lock()
        .unwrap()
        .extend([(manager, None), (gone, None)]);
    let seen = |id| rig.rescoped.lock().unwrap()[&id].clone();
    let grant = |capabilities: &[Capability]| Command::SetBackend {
        thread: manager,
        backend: CodingBackend::Native,
        capabilities: capabilities.iter().copied().collect(),
    };
    assert_eq!(
        rig.run(grant(&[Capability::ReadWorkspace])).await,
        Reply::Ok
    );
    assert_eq!(
        seen(manager),
        Some(Some([Capability::ReadWorkspace].into()))
    );
    assert_eq!(
        seen(gone),
        Some(None),
        "a missing conversation has no scope"
    );
    assert_eq!(rig.run(grant(&[])).await, Reply::Ok);
    assert_eq!(seen(manager), Some(Some(BTreeSet::new())));
}

#[tokio::test]
async fn a_grant_changed_during_a_call_is_rescoped_once_the_call_returns() {
    let api = ProviderApi::OpenAi;
    let name = format!("held_{}", nonce().replace('-', "_"));
    let mut rig = rig(
        api,
        vec![tool(
            &name,
            ToolPermission::Allow,
            &[Capability::ReadWorkspace],
        )],
        None,
    )
    .await;
    let workspace = tempfile::tempdir().unwrap();
    let Reply::Project(project) = rig
        .run(Command::CreateProject {
            name: nonce(),
            root: workspace.path().to_string_lossy().into(),
        })
        .await
    else {
        panic!("project creation failed");
    };
    let manager = project.manager;
    let configured = rig
        .run(Command::ConfigureThread {
            id: manager,
            mode: Mode::Code,
            model: Some(rig.model.clone()),
            subagent_models: [rig.model.clone()].into(),
        })
        .await;
    assert!(matches!(configured, Reply::Thread(_)), "{configured:?}");
    let grant = |capabilities: &[Capability]| Command::SetBackend {
        thread: manager,
        backend: CodingBackend::Native,
        capabilities: capabilities.iter().copied().collect(),
    };
    assert_eq!(
        rig.run(grant(&[Capability::ReadWorkspace, Capability::Execute]))
            .await,
        Reply::Ok
    );
    rig.thread = manager;
    let child = {
        delegate(
            &mut rig,
            Mode::Code,
            &[Capability::ReadWorkspace, Capability::Execute],
            &name,
        )
        .await;
        rig.thread
    };
    let task = rig.send();
    let first = rig.next_request().await;
    let call = Call::new(&name, json!({}));
    first
        .respond(calls_response(api, std::slice::from_ref(&call), ""))
        .await;
    let (scope, _) = wait_for_call(&mut rig).await;
    assert!(scope.capabilities.contains(&Capability::Execute));
    // The parent loses a grant while its child's call runs: the tool itself
    // is still permitted, but what the call started must be looked at again.
    assert_eq!(
        rig.run(grant(&[Capability::ReadWorkspace])).await,
        Reply::Ok
    );
    rig.rescoped.lock().unwrap().insert(child, None);
    rig.release.notify_one();
    let second = rig.next_request().await;
    assert_eq!(
        rig.rescoped.lock().unwrap()[&child],
        Some(Some([Capability::ReadWorkspace].into())),
        "no rescope after the call returned"
    );
    second.respond(text_response(api, &nonce())).await;
    assert_eq!(finished(task).await.state, RunState::Complete);
}

#[tokio::test]
async fn a_parent_lowering_its_budget_limits_its_child() {
    let api = ProviderApi::OpenAi;
    let name = nonce();
    let mut rig = rig(api, vec![tool(&name, ToolPermission::Allow, &[])], None).await;
    let parent = delegate(&mut rig, Mode::Chat, &[], &name).await;
    let set = |budget| Command::SetToolBudget {
        thread: parent,
        budget,
    };
    // Calls: the child's own budget allows both, the parent's only one.
    let parent_budget = ToolBudget {
        tokens: None,
        rounds: ToolBudget::default().rounds,
        tool_calls: 1,
    };
    assert_eq!(rig.run(set(parent_budget)).await, Reply::Ok);
    assert!(rig.thread().tool_budget.tool_calls > 1);
    let task = rig.send();
    let request = rig.next_request().await;
    let calls = [
        Call::new(&name, json!({"value": nonce()})),
        Call::new(&name, json!({"value": nonce()})),
    ];
    request.respond(calls_response(api, &calls, "")).await;
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Failed);
    assert!(
        thread
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("1 tool calls"),
        "{:?}",
        thread.error
    );
    assert_eq!(rig.executed().len(), 1);

    // Rounds, independently of calls.
    let parent_budget = ToolBudget {
        tokens: None,
        rounds: 1,
        tool_calls: ToolBudget::default().tool_calls,
    };
    assert_eq!(rig.run(set(parent_budget)).await, Reply::Ok);
    let task = rig.send();
    let request = rig.next_request().await;
    request
        .respond(calls_response(
            api,
            &[Call::new(&name, json!({"value": nonce()}))],
            "",
        ))
        .await;
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Failed);
    assert!(
        thread
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("1 model rounds"),
        "{:?}",
        thread.error
    );
    assert!(rig.executed().is_empty());
}

#[tokio::test]
async fn a_tool_reported_error_reaches_the_user_with_its_explanation() {
    let api = ProviderApi::Anthropic;
    let name = format!("fail_{}", nonce());
    let mut rig = rig(api, vec![tool(&name, ToolPermission::Allow, &[])], None).await;
    rig.permit(&name, ToolPermission::Allow).await;
    let task = rig.send();
    let first = rig.next_request().await;
    let value = nonce();
    let call = Call::new(&name, json!({"value": value}));
    first
        .respond(calls_response(api, std::slice::from_ref(&call), ""))
        .await;
    let second = rig.next_request().await;
    let explanation = format!("{}:{}", rig.marker, json!({"value": value}));
    assert_eq!(
        result_for(api, &second.body, &call.id),
        Some(explanation.clone())
    );
    second.respond(text_response(api, &nonce())).await;
    let thread = finished(task).await;
    let recorded = &calls_of(&thread)[0];
    assert_eq!(recorded.state, ToolCallState::Failed);
    let report = recorded.error.as_ref().unwrap();
    // Typed: the gateway's report first, the tool's own words as its cause.
    assert!(report.message.contains("reported an error"), "{report:?}");
    let cause = report.source.as_deref().expect("the tool's explanation");
    assert_eq!(cause.message, explanation);
}

#[tokio::test]
async fn the_context_shows_the_last_request_while_cost_counts_the_whole_run() {
    let api = ProviderApi::Anthropic;
    let name = nonce();
    let mut rig = rig(api, vec![tool(&name, ToolPermission::Allow, &[])], None).await;
    rig.permit(&name, ToolPermission::Allow).await;
    let task = rig.send();
    let (first_in, first_out, last_in, last_out) = (count(), count(), count(), count());
    let first = rig.next_request().await;
    first
        .respond(with_usage(
            calls_response(api, &[Call::new(&name, json!({}))], ""),
            first_in,
            first_out,
        ))
        .await;
    let second = rig.next_request().await;
    second
        .respond(with_usage(text_response(api, &nonce()), last_in, last_out))
        .await;
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Complete);
    let last = thread.usage.unwrap();
    assert_eq!(
        (last.input_tokens, last.output_tokens),
        (Some(last_in), Some(last_out))
    );
    let total = thread.run_usage.unwrap();
    assert_eq!(
        (total.input_tokens, total.output_tokens),
        (Some(first_in + last_in), Some(first_out + last_out))
    );
    // A run that fails reports no cost rather than the previous run's.
    let task = rig.send();
    let failing = rig.next_request().await;
    let cut_off = text_response(api, &nonce()).replace("message_stop", "ping");
    failing.respond(with_usage(cut_off, count(), count())).await;
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Failed);
    assert_eq!(thread.run_usage, None);
}

#[tokio::test]
async fn calls_that_do_not_fit_in_storage_are_settled_and_the_run_goes_on() {
    let api = ProviderApi::OpenAi;
    // No tool is offered: every call is refused cheaply, so only the
    // recorded arguments fill the conversation.
    let mut rig = rig(api, vec![], None).await;
    assert_eq!(
        rig.run(Command::SetToolBudget {
            thread: rig.thread,
            budget: ToolBudget {
                tokens: None,
                rounds: 8,
                tool_calls: ToolBudget::MAX,
            },
        })
        .await,
        Reply::Ok
    );
    let large = |n: usize| -> Vec<Call> {
        (0..n)
            .map(|_| {
                // The margin leaves the first batch room for the rest of the
                // stored state, such as bundled extension manifests.
                let filler = "a".repeat(a206_runtime::tool_calls::MAX_ARGUMENTS - 9 * 1024);
                Call::new(&nonce(), json!({"value": format!("{}{filler}", nonce())}))
            })
            .collect()
    };
    let task = rig.send();
    let first = rig.next_request().await;
    first
        .respond(calls_response(
            api,
            &large(a206_runtime::tool_calls::MAX_CALLS),
            "",
        ))
        .await;
    let second = rig.next_request().await;
    let overflow = large(16);
    second.respond(calls_response(api, &overflow, "")).await;
    let third = rig.next_request().await;
    for call in &overflow {
        assert!(
            result_for(api, &third.body, &call.id).is_some(),
            "every call gets a result"
        );
    }
    // Small calls fill the conversation to within a few hundred bytes of
    // what calls may use.
    let small: Vec<Call> = (0..a206_runtime::tool_calls::MAX_CALLS)
        .map(|_| Call::new(&nonce(), json!({"value": nonce()})))
        .collect();
    third.respond(calls_response(api, &small, "")).await;
    let fourth = rig.next_request().await;
    // The longest reply the provider accepts still fits after the calls.
    let mut reply = nonce();
    reply.push_str(&"b".repeat(a206_runtime::provider::MAX_REPLY_TEXT - reply.len()));
    fourth.respond(text_response(api, &reply)).await;
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Complete, "{:?}", thread.error);
    assert_eq!(thread.messages.last().unwrap().text, reply);
    let recorded = calls_of(&thread);
    let settled: Vec<_> = recorded
        .iter()
        .filter(|c| c.state == ToolCallState::Failed)
        .collect();
    assert!(!settled.is_empty(), "nothing exceeded the storage limit");
    for call in settled {
        assert_eq!(error_kind(call), ErrorKind::Storage);
        assert!(call.arguments.is_empty());
        assert!(overflow.iter().any(|c| c.id == call.id));
    }
    // The conversation is still stored and readable.
    assert!(rig.gateway.snapshot().is_ok());
}

#[tokio::test]
async fn a_message_is_admitted_only_with_room_for_its_reply() {
    let api = ProviderApi::OpenAi;
    let mut rig = rig(api, vec![], None).await;
    let mut accepted = 0;
    // Messages shrink after each refusal, so the conversation ends within a
    // small message of the admission limit.
    let mut size = 120 * 1024;
    loop {
        let before = rig.thread().messages.len();
        let mut text = nonce();
        text.push_str(&"c".repeat(size));
        let gateway = rig.gateway.clone();
        let thread = rig.thread;
        let mut task = tokio::spawn(async move {
            gateway
                .request(a206_protocol::Request {
                    id: Id::new_v4(),
                    body: Command::Send {
                        thread,
                        text,
                        attachments: vec![],
                    },
                })
                .await
                .body
        });
        // An accepted message reaches the provider; a refused one replies
        // at once. Either way nothing waits on time.
        tokio::select! {
            request = rig.requests.recv() => {
                let request = request.unwrap();
                let reply = nonce();
                request.respond(text_response(api, &reply)).await;
                let thread = finished(task).await;
                assert_eq!(thread.state, RunState::Complete, "{:?}", thread.error);
                assert_eq!(thread.messages.last().unwrap().text, reply);
                accepted += 1;
            }
            refused = &mut task => {
                let refused = refused.unwrap();
                assert!(
                    matches!(refused, Reply::Error(ref r) if r.kind == ErrorKind::Storage),
                    "{refused:?}"
                );
                // A refused message is not stored and the conversation is
                // left as it was.
                let thread = rig.thread();
                assert_eq!(thread.messages.len(), before);
                assert_eq!(thread.state, RunState::Complete);
                if size < 1024 {
                    break;
                }
                size /= 2;
            }
        }
    }
    assert!(accepted > 0);
}

#[tokio::test]
async fn output_that_must_be_whole_fails_rather_than_being_cut() {
    let api = ProviderApi::OpenAi;
    let whole = nonce();
    let cut = nonce();
    let mut tools = vec![
        tool(&whole, ToolPermission::Allow, &[]),
        tool(&cut, ToolPermission::Allow, &[]),
    ];
    tools[0].whole_output = true;
    let mut rig = rig(api, tools, None).await;
    for name in [&whole, &cut] {
        rig.permit(name, ToolPermission::Allow).await;
    }
    // The echo host returns its arguments with a prefix, so arguments near
    // their limit make an output past the output limit.
    let arguments = |id: &str| {
        let filler = "o".repeat(a206_runtime::tool_calls::MAX_ARGUMENTS - 64);
        json!({"value": format!("{id}{filler}")})
    };
    let calls = [
        Call::new(&whole, arguments(&nonce())),
        Call::new(&cut, arguments(&nonce())),
    ];
    let task = rig.send();
    let first = rig.next_request().await;
    first.respond(calls_response(api, &calls, "")).await;
    let second = rig.next_request().await;
    let truncated = result_for(api, &second.body, &calls[1].id).unwrap();
    assert!(
        truncated.contains("[output truncated by the gateway"),
        "the output was not over the limit"
    );
    let refused = result_for(api, &second.body, &calls[0].id).unwrap();
    assert!(refused.contains("only useful whole"), "{refused}");
    second.respond(text_response(api, &nonce())).await;
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Complete, "{:?}", thread.error);
    let recorded = calls_of(&thread);
    let record = |id: &str| recorded.iter().find(|c| c.id == id).unwrap();
    assert_eq!(record(&calls[0].id).state, ToolCallState::Failed);
    assert_eq!(error_kind(record(&calls[0].id)), ErrorKind::Storage);
    // The model is told why; none of the tool's own output is kept.
    let kept = record(&calls[0].id).output.as_deref().unwrap_or_default();
    assert!(!kept.contains("oooo"), "{kept}");
    assert_eq!(record(&calls[1].id).state, ToolCallState::Succeeded);
}

// ── Output rules ────────────────────────────────────────────────────────────

impl Rig {
    /// Add a user output rules file to the gateway's state directory.
    fn write_rules(&self, text: &str) -> std::path::PathBuf {
        let directory = self._state.path().join(a206_runtime::gateway::OUTPUT_RULES);
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("{}.toml", nonce()));
        std::fs::write(&path, text).unwrap();
        path
    }
}

#[tokio::test]
async fn output_rules_shorten_what_the_model_sees_and_the_call_records_it() {
    let api = ProviderApi::Anthropic;
    let (tool_name, failing) = (
        format!("lines_{}", nonce()),
        format!("fail_lines_{}", nonce()),
    );
    let mut rig = rig(
        api,
        vec![
            tool(&tool_name, ToolPermission::Allow, &[]),
            tool(&failing, ToolPermission::Allow, &[]),
        ],
        None,
    )
    .await;
    for name in [&tool_name, &failing] {
        rig.permit(name, ToolPermission::Allow).await;
    }
    let (rule, program, noise) = (nonce(), nonce(), nonce());
    rig.write_rules(&format!(
        "[[rule]]\nid = \"{rule}\"\n[rule.match]\ntools = [\"{tool_name}\", \"{failing}\"]\nargv0 = [\"{program}\"]\n[rule.filters]\nskip = ['^{noise}']\n[[rule.counters]]\nname = \"kept\"\npattern = '^keep'\n"
    ));
    let value = format!("keep {}\n{noise} {}\nkeep {}\n", nonce(), nonce(), nonce());
    let lines: Vec<&str> = value.lines().collect();
    let reduced = format!(
        "{}\n{}\n[output rule `{rule}` kept 2 of 3 lines; kept: 2]",
        lines[0], lines[2]
    );
    let calls = [
        // The rule's program, after a `cd`, with its output piped on.
        Call::new(
            &tool_name,
            json!({"command": format!("cd x && {program} run | tee log"), "value": value}),
        ),
        // Another program: no rule applies.
        Call::new(&tool_name, json!({"command": nonce(), "value": value})),
        // A reported failure is reduced too.
        Call::new(&failing, json!({"command": program, "value": value})),
    ];
    let task = rig.send();
    let first = rig.next_request().await;
    first.respond(calls_response(api, &calls, "")).await;
    let second = rig.next_request().await;
    assert_eq!(
        result_for(api, &second.body, &calls[0].id),
        Some(reduced.clone())
    );
    assert_eq!(
        result_for(api, &second.body, &calls[1].id),
        Some(value.clone())
    );
    assert_eq!(
        result_for(api, &second.body, &calls[2].id),
        Some(reduced.clone())
    );
    second.respond(text_response(api, &nonce())).await;
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Complete, "{:?}", thread.error);
    let recorded = calls_of(&thread);
    let record = |id: &str| recorded.iter().find(|c| c.id == id).unwrap();
    let expected = OutputReduction {
        rule: rule.clone(),
        user_rule: true,
        original_bytes: value.len() as u64,
        counts: vec![OutputCount {
            name: "kept".into(),
            count: 2,
        }],
    };
    assert_eq!(record(&calls[0].id).reduction.as_ref(), Some(&expected));
    assert_eq!(
        record(&calls[0].id).output.as_deref(),
        Some(reduced.as_str())
    );
    assert_eq!(record(&calls[1].id).reduction, None);
    let failed = record(&calls[2].id);
    assert_eq!(failed.state, ToolCallState::Failed);
    assert_eq!(failed.reduction.as_ref(), Some(&expected));
    assert_eq!(failed.output.as_deref(), Some(reduced.as_str()));
}

#[tokio::test]
async fn output_that_must_be_whole_is_never_reduced() {
    let api = ProviderApi::OpenAi;
    let name = format!("lines_{}", nonce());
    let mut tools = vec![tool(&name, ToolPermission::Allow, &[])];
    tools[0].whole_output = true;
    let mut rig = rig(api, tools, None).await;
    rig.permit(&name, ToolPermission::Allow).await;
    let noise = nonce();
    rig.write_rules(&format!(
        "[[rule]]\nid = \"{}\"\n[rule.filters]\nskip = ['^{noise}']\n",
        nonce()
    ));
    let value = format!("{}\n{noise}\n", nonce());
    let call = Call::new(&name, json!({"value": value}));
    let task = rig.send();
    let first = rig.next_request().await;
    first
        .respond(calls_response(api, std::slice::from_ref(&call), ""))
        .await;
    let second = rig.next_request().await;
    assert_eq!(result_for(api, &second.body, &call.id), Some(value.clone()));
    second.respond(text_response(api, &nonce())).await;
    let thread = finished(task).await;
    assert_eq!(calls_of(&thread)[0].reduction, None);
}

// ── Credential removal ──────────────────────────────────────────────────────

#[tokio::test]
async fn credentials_in_tool_results_never_reach_the_model_or_the_record() {
    let api = ProviderApi::OpenAi;
    let names = [
        format!("lines_{}", nonce()),
        format!("fail_lines_{}", nonce()),
        format!("broken_lines_{}", nonce()),
        format!("whole_lines_{}", nonce()),
    ];
    let mut tools: Vec<_> = names
        .iter()
        .map(|name| tool(name, ToolPermission::Allow, &[]))
        .collect();
    tools[3].whole_output = true;
    let mut rig = rig(api, tools, None).await;
    for name in &names {
        rig.permit(name, ToolPermission::Allow).await;
    }
    // A rule that would count the token's line, were it still there.
    let (program, noise) = (nonce(), nonce());
    rig.write_rules(&format!(
        "[[rule]]\nid = \"{}\"\n[rule.match]\nargv0 = [\"{program}\"]\n[rule.filters]\nskip = ['^{noise}']\n[[rule.counters]]\nname = \"tokens\"\npattern = 'ghp_'\n",
        nonce()
    ));
    // The model passes the text in, so the token appears in its own
    // arguments; only what the tools return is checked.
    let token = format!("ghp_{}{}", nonce(), nonce());
    let (before, after) = (nonce(), nonce());
    let value = format!("{before} {token}\n{noise}\n{after}\n");
    let clean = format!("{before} [redacted github_token]\n{noise}\n{after}\n");
    let calls: Vec<_> = names
        .iter()
        .map(|name| Call::new(name, json!({"command": program, "value": value})))
        .collect();
    let task = rig.send();
    let first = rig.next_request().await;
    first.respond(calls_response(api, &calls, "")).await;
    let second = rig.next_request().await;
    for call in &calls {
        let seen = result_for(api, &second.body, &call.id).unwrap();
        assert!(!seen.contains(&token), "{seen}");
        assert!(seen.contains("[redacted github_token]"), "{seen}");
    }
    second.respond(text_response(api, &nonce())).await;
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Complete, "{:?}", thread.error);
    let recorded = calls_of(&thread);
    let record = |id: &str| recorded.iter().find(|c| c.id == id).unwrap();
    let found = vec![OutputCount {
        name: "github_token".into(),
        count: 1,
    }];
    for call in &calls {
        let call = record(&call.id);
        assert_eq!(call.redactions, found, "{}", call.tool);
        assert!(!call.output.as_deref().unwrap().contains(&token));
        let report = call.error.as_ref().map(ErrorReport::detail);
        assert!(!report.unwrap_or_default().contains(&token));
    }
    // The rule ran on the cleaned text: nothing left for it to count.
    let reduction = record(&calls[0].id).reduction.clone().unwrap();
    assert_eq!(reduction.counts[0].count, 0);
    assert!(
        record(&calls[0].id)
            .output
            .as_deref()
            .unwrap()
            .starts_with(&format!(
                "{before} [redacted github_token]\n{after}\n[output rule"
            ))
    );
    assert_eq!(record(&calls[3].id).output.as_deref(), Some(clean.as_str()));
    let broken = record(&calls[2].id);
    assert_eq!(broken.state, ToolCallState::Failed);
    assert!(broken.error.is_some());
}

#[tokio::test]
async fn a_credential_an_output_rule_joins_is_removed_too() {
    let api = ProviderApi::Anthropic;
    let name = format!("lines_{}", nonce());
    let mut rig = rig(api, vec![tool(&name, ToolPermission::Allow, &[])], None).await;
    rig.permit(&name, ToolPermission::Allow).await;
    rig.write_rules(&format!(
        "[[rule]]\nid = \"{}\"\n[rule.match]\ntools = [\"{name}\"]\n[rule.transforms]\nstrip_ansi = true\n",
        nonce()
    ));
    // A colour change inside the token hides its shape until the rule
    // strips the escape.
    let (head, tail) = (nonce(), nonce());
    let value = format!("ghp_{head}\u{1b}[0m{tail}\n");
    let call = Call::new(&name, json!({"value": value}));
    let task = rig.send();
    let first = rig.next_request().await;
    first
        .respond(calls_response(api, std::slice::from_ref(&call), ""))
        .await;
    let second = rig.next_request().await;
    let seen = result_for(api, &second.body, &call.id).unwrap();
    assert!(!seen.contains(&tail), "{seen}");
    assert!(seen.contains("[redacted github_token]"), "{seen}");
    second.respond(text_response(api, &nonce())).await;
    let thread = finished(task).await;
    let call = &calls_of(&thread)[0];
    assert!(call.reduction.is_some());
    assert_eq!(
        call.redactions,
        [OutputCount {
            name: "github_token".into(),
            count: 1
        }]
    );
}

#[tokio::test]
async fn an_invalid_output_rule_file_fails_the_run_before_the_model_is_asked() {
    let api = ProviderApi::Gemini;
    let mut rig = rig(api, vec![], None).await;
    let path = rig.write_rules(&format!("[[rule]]\nid = \"{}\"\nunknown = 1\n", nonce()));
    let thread = finished(rig.send()).await;
    assert_eq!(thread.state, RunState::Failed);
    let report = thread.error.expect("the run's failure");
    assert_eq!(report.kind, ErrorKind::InvalidRequest, "{report:?}");
    assert!(
        report.message.contains(&path.display().to_string()),
        "{report:?}"
    );
    // The parser's own explanation travels as the cause.
    assert!(report.source.is_some(), "{report:?}");
    assert!(rig.requests.try_recv().is_err(), "the model was asked");
}

// ── Subagents ───────────────────────────────────────────────────────────────

use a206_runtime::delegation_tool as subagents;

/// The next two requests: the parent's, which carries `call`'s result, and
/// the one from the subagent's run.
async fn parent_and_child(rig: &mut Rig, api: ProviderApi, call: &str) -> (Opened, Opened) {
    let first = rig.next_request().await;
    let second = rig.next_request().await;
    if result_for(api, &first.body, call).is_some() {
        (first, second)
    } else {
        (second, first)
    }
}
fn result_json(api: ProviderApi, body: &Value, call: &str) -> Value {
    serde_json::from_str(&result_for(api, body, call).unwrap()).unwrap()
}
fn thread_of(rig: &Rig, id: Id) -> Thread {
    let state = rig.gateway.snapshot().unwrap();
    state.threads.into_iter().find(|t| t.id == id).unwrap()
}

#[tokio::test]
async fn subagents_run_their_task_with_no_more_than_their_parents_permissions() {
    let api = ProviderApi::OpenAi;
    let name = nonce();
    let mut rig = rig(api, vec![tool(&name, ToolPermission::Allow, &[])], None).await;
    for tool in [name.as_str(), subagents::SPAWN, subagents::WAIT] {
        rig.permit(tool, ToolPermission::Allow).await;
    }
    let task = rig.send();
    let first = rig.next_request().await;
    let mut offered_first = offered(api, &first.body);
    offered_first.sort();
    let mut expected = vec![
        name.clone(),
        subagents::SPAWN.into(),
        subagents::WAIT.into(),
    ];
    expected.sort();
    assert_eq!(offered_first, expected);
    let instructions = nonce();
    let spawn = Call::new(subagents::SPAWN, json!({"task": instructions}));
    first
        .respond(calls_response(api, std::slice::from_ref(&spawn), &nonce()))
        .await;

    let (parent, child) = parent_and_child(&mut rig, api, &spawn.id).await;
    // The subagent sees its task, and the parent's tools but not delegation.
    assert!(child.body.to_string().contains(&instructions));
    assert_eq!(offered(api, &child.body), vec![name.clone()]);
    let answer = nonce();
    child.respond(text_response(api, &answer)).await;
    let spawned = result_json(api, &parent.body, &spawn.id);
    let id: Id = spawned["thread"].as_str().unwrap().parse().unwrap();
    assert_eq!(spawned["state"], "running");

    let waiting = Call::new(subagents::WAIT, json!({"thread": id}));
    parent
        .respond(calls_response(
            api,
            std::slice::from_ref(&waiting),
            &nonce(),
        ))
        .await;
    let third = rig.next_request().await;
    let waited = result_json(api, &third.body, &waiting.id);
    assert_eq!(waited["state"], "complete", "{waited}");
    assert_eq!(waited["reply"], answer.as_str());
    third.respond(text_response(api, &nonce())).await;
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Complete, "{:?}", thread.error);

    let child = thread_of(&rig, id);
    assert_eq!(child.parent, Some(rig.thread));
    assert_eq!(child.model.as_ref(), Some(&rig.model));
    assert_eq!(
        child.tool_permissions.keys().cloned().collect::<Vec<_>>(),
        vec![name]
    );
}

#[tokio::test]
async fn a_subagent_started_from_a_profile_takes_its_mode_model_and_instructions() {
    let api = ProviderApi::OpenAi;
    let mut rig = rig(api, vec![], None).await;
    for tool in [subagents::SPAWN, subagents::WAIT] {
        rig.permit(tool, ToolPermission::Allow).await;
    }
    let profiles = rig._state.path().join(a206_core::profiles::DIRECTORY);
    std::fs::create_dir(&profiles).unwrap();
    let id = "reviewer";
    let instructions = nonce();
    let description = nonce();
    std::fs::write(
        profiles.join(format!("{id}.toml")),
        format!(
            "name = \"Reviewer\"\ndescription = \"{description}\"\nmode = \"plan\"\nmodel = \"{}\"\ninstructions = \"{instructions}\"",
            rig.model.model
        ),
    )
    .unwrap();
    std::fs::write(profiles.join("broken.toml"), "name = ").unwrap();

    let task = rig.send();
    let first = rig.next_request().await;
    // The model is offered the usable profiles, with what each is for.
    let offered = first.body.to_string();
    assert!(offered.contains(&description), "{offered}");
    assert!(!offered.contains("broken"));
    let conflicting = Call::new(
        subagents::SPAWN,
        json!({"task": nonce(), "profile": id, "mode": "code"}),
    );
    let unknown = Call::new(
        subagents::SPAWN,
        json!({"task": nonce(), "profile": "broken"}),
    );
    let spawn = Call::new(subagents::SPAWN, json!({"task": nonce(), "profile": id}));
    first
        .respond(calls_response(
            api,
            &[conflicting.clone(), unknown.clone(), spawn.clone()],
            &nonce(),
        ))
        .await;
    let (parent, child) = parent_and_child(&mut rig, api, &spawn.id).await;
    // The profile's instructions join the subagent's prompt, not the parent's.
    assert!(child.body.to_string().contains(&instructions));
    assert!(!parent.body.to_string().contains(&instructions));
    child.respond(text_response(api, &nonce())).await;
    let spawned = result_json(api, &parent.body, &spawn.id);
    assert_eq!(spawned["profile"], id);
    assert_eq!(spawned["mode"], "plan");
    parent.respond(text_response(api, &nonce())).await;
    let thread = finished(task).await;
    let calls = calls_of(&thread);
    assert_eq!(error_kind(&calls[0]), ErrorKind::InvalidRequest);
    assert_eq!(error_kind(&calls[1]), ErrorKind::InvalidRequest);

    let child: Id = spawned["thread"].as_str().unwrap().parse().unwrap();
    let child = thread_of(&rig, child);
    assert_eq!(child.mode, Mode::Plan);
    assert_eq!(child.model.as_ref(), Some(&rig.model));
    let profile = child.profile.expect("the subagent records its profile");
    assert_eq!(
        (profile.id.as_str(), profile.instructions.as_str()),
        (id, instructions.as_str())
    );
    let subagents: Vec<_> = rig
        .gateway
        .snapshot()
        .unwrap()
        .threads
        .into_iter()
        .filter(|t| t.parent == Some(rig.thread))
        .collect();
    assert_eq!(subagents.len(), 1, "refused requests created nothing");

    // The same checks hold for a client's CreateSubagent.
    let refused = rig
        .run(Command::CreateSubagent {
            parent: rig.thread,
            title: nonce(),
            mode: Mode::Code,
            model: rig.model.clone(),
            requested: Default::default(),
            profile: Some(id.into()),
        })
        .await;
    let Reply::Error(report) = refused else {
        panic!("a mode other than the profile's was accepted: {refused:?}");
    };
    assert_eq!(report.kind, ErrorKind::InvalidRequest);
    let Reply::AgentProfiles(listed) = rig.run(Command::AgentProfiles).await else {
        panic!("profiles unavailable");
    };
    assert_eq!(listed.profiles.len(), 1);
    assert_eq!(listed.problems[0].file, "broken.toml");
}

#[tokio::test]
async fn cancelling_a_run_cancels_its_subagents() {
    let api = ProviderApi::OpenAi;
    let mut rig = rig(api, vec![], None).await;
    for tool in [subagents::SPAWN, subagents::WAIT] {
        rig.permit(tool, ToolPermission::Allow).await;
    }
    let task = rig.send();
    let first = rig.next_request().await;
    let spawn = Call::new(subagents::SPAWN, json!({"task": nonce()}));
    first
        .respond(calls_response(api, std::slice::from_ref(&spawn), &nonce()))
        .await;
    let (parent, _child) = parent_and_child(&mut rig, api, &spawn.id).await;
    let id: Id = result_json(api, &parent.body, &spawn.id)["thread"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    // The parent waits on a subagent whose model never answers.
    let waiting = Call::new(
        subagents::WAIT,
        json!({"thread": id, "seconds": subagents::MAX_WAIT}),
    );
    parent
        .respond(calls_response(
            api,
            std::slice::from_ref(&waiting),
            &nonce(),
        ))
        .await;
    tokio::time::timeout(PATIENCE, async {
        while thread_of(&rig, id).state != RunState::Running {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(wait("the subagent to run"));

    let cancel = rig.run(Command::CancelRun { thread: rig.thread }).await;
    assert_eq!(cancel, Reply::Ok);
    assert_eq!(finished(task).await.state, RunState::Cancelled);
    tokio::time::timeout(PATIENCE, async {
        while thread_of(&rig, id).state != RunState::Cancelled {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(wait("the subagent to be cancelled"));
}

#[tokio::test]
async fn subagent_tools_stay_within_permitted_models_and_the_callers_own_subagents() {
    let api = ProviderApi::OpenAi;
    let mut rig = rig(api, vec![], None).await;
    for tool in [subagents::SPAWN, subagents::STATUS] {
        rig.permit(tool, ToolPermission::Allow).await;
    }
    let task = rig.send();
    let first = rig.next_request().await;
    let stranger = Call::new(subagents::STATUS, json!({"thread": Id::new_v4()}));
    let unpermitted = Call::new(subagents::SPAWN, json!({"task": nonce(), "model": nonce()}));
    first
        .respond(calls_response(
            api,
            &[stranger.clone(), unpermitted.clone()],
            &nonce(),
        ))
        .await;
    let second = rig.next_request().await;
    assert!(result_for(api, &second.body, &stranger.id).is_some());
    second.respond(text_response(api, &nonce())).await;
    let thread = finished(task).await;
    let calls = calls_of(&thread);
    assert_eq!(calls.len(), 2);
    assert_eq!(error_kind(&calls[0]), ErrorKind::NotFound);
    assert_eq!(error_kind(&calls[1]), ErrorKind::PermissionDenied);
    assert!(
        rig.gateway
            .snapshot()
            .unwrap()
            .threads
            .iter()
            .all(|t| t.parent != Some(rig.thread)),
        "no subagent was created"
    );

    // Without subagent models the tools are not offered at all.
    let configured = rig
        .run(Command::ConfigureThread {
            id: rig.thread,
            mode: Mode::Chat,
            model: Some(rig.model.clone()),
            subagent_models: Default::default(),
        })
        .await;
    assert!(matches!(configured, Reply::Thread(_)));
    let task = rig.send();
    let first = rig.next_request().await;
    assert!(offered(api, &first.body).is_empty());
    first.respond(text_response(api, &nonce())).await;
    finished(task).await;
}

// ── Steering ────────────────────────────────────────────────────────────────

impl Rig {
    async fn steer(&self, thread: Id, text: &str) -> Reply {
        self.run(Command::Steer {
            thread,
            text: text.into(),
        })
        .await
    }
}
fn refused(reply: Reply, kind: ErrorKind) {
    match reply {
        Reply::Error(report) => assert_eq!(report.kind, kind, "{report:?}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn a_steered_message_is_answered_before_the_run_ends() {
    for api in PROTOCOLS {
        let mut rig = rig(api, vec![], None).await;
        let task = rig.send();
        let first = rig.next_request().await;
        // Steered while the model is still replying: it waits for the round.
        let steered = nonce();
        assert_eq!(rig.steer(rig.thread, &steered).await, Reply::Ok);
        assert_eq!(rig.thread().steering.len(), 1);
        assert!(!first.body.to_string().contains(&steered));
        let reply = nonce();
        first.respond(text_response(api, &reply)).await;

        // The run continues instead of ending, with the message after the reply.
        let second = rig.next_request().await;
        let body = second.body.to_string();
        let (at_reply, at_steered) = (body.find(&reply), body.find(&steered));
        assert!(
            at_reply.is_some() && at_steered > at_reply,
            "{api:?}: {body}"
        );
        let answer = nonce();
        second.respond(text_response(api, &answer)).await;
        let thread = finished(task).await;
        assert_eq!(thread.state, RunState::Complete, "{:?}", thread.error);
        assert!(thread.steering.is_empty());
        let texts: Vec<_> = thread
            .messages
            .iter()
            .map(|m| (m.role.clone(), m.text.as_str()))
            .collect();
        assert_eq!(
            texts[texts.len() - 3..],
            [
                (Role::Assistant, reply.as_str()),
                (Role::User, steered.as_str()),
                (Role::Assistant, answer.as_str())
            ],
            "{api:?}"
        );
        // An ended run takes no more messages.
        refused(rig.steer(rig.thread, &nonce()).await, ErrorKind::Conflict);
    }
}

#[tokio::test]
async fn a_steered_message_follows_the_tool_results_of_its_round() {
    let api = ProviderApi::Anthropic;
    let name = nonce();
    let mut rig = rig(api, vec![tool(&name, ToolPermission::Allow, &[])], None).await;
    rig.permit(&name, ToolPermission::Allow).await;
    let task = rig.send();
    let first = rig.next_request().await;
    let steered = nonce();
    assert_eq!(rig.steer(rig.thread, &steered).await, Reply::Ok);
    let call = Call::new(&name, json!({"value": nonce()}));
    first
        .respond(calls_response(api, std::slice::from_ref(&call), &nonce()))
        .await;
    let second = rig.next_request().await;
    let history = history_of(api, &second.body);
    let result = history
        .iter()
        .position(|m| m.to_string().contains(&call.id) && m["role"] == "user")
        .expect("tool result");
    let message = history
        .iter()
        .position(|m| m.to_string().contains(&steered))
        .expect("steered message");
    assert!(message > result, "{history:?}");
    second.respond(text_response(api, &nonce())).await;
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Complete, "{:?}", thread.error);
    assert_eq!(calls_of(&thread)[0].state, ToolCallState::Succeeded);
}

#[tokio::test]
async fn steering_is_refused_without_a_run_and_kept_when_a_run_is_cancelled() {
    let api = ProviderApi::OpenAi;
    let mut rig = rig(api, vec![], None).await;
    refused(rig.steer(rig.thread, &nonce()).await, ErrorKind::Conflict);
    refused(rig.steer(Id::new_v4(), &nonce()).await, ErrorKind::NotFound);
    let task = rig.send();
    let _first = rig.next_request().await;
    refused(rig.steer(rig.thread, " ").await, ErrorKind::InvalidRequest);
    let steered = nonce();
    assert_eq!(rig.steer(rig.thread, &steered).await, Reply::Ok);
    assert_eq!(
        rig.run(Command::CancelRun { thread: rig.thread }).await,
        Reply::Ok
    );
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Cancelled);
    // The user's words are never dropped: they stay after the cancelled run.
    assert!(thread.steering.is_empty());
    let last = thread.messages.last().unwrap();
    assert_eq!(
        (last.role.clone(), last.text.as_str()),
        (Role::User, steered.as_str())
    );
    refused(rig.steer(rig.thread, &nonce()).await, ErrorKind::Conflict);
}

#[tokio::test]
async fn a_steered_message_left_by_a_failed_run_follows_its_failure() {
    let api = ProviderApi::OpenAi;
    let mut rig = rig(api, vec![], None).await;
    // One round: the reply uses it, so the steered message cannot be answered.
    let budget = ToolBudget {
        rounds: 1,
        tool_calls: 1,
        tokens: None,
    };
    let reply = rig
        .run(Command::SetToolBudget {
            thread: rig.thread,
            budget,
        })
        .await;
    assert_eq!(reply, Reply::Ok);
    let task = rig.send();
    let first = rig.next_request().await;
    let steered = nonce();
    assert_eq!(rig.steer(rig.thread, &steered).await, Reply::Ok);
    let answer = nonce();
    first.respond(text_response(api, &answer)).await;
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Failed);
    assert!(thread.steering.is_empty());
    let texts: Vec<_> = thread
        .messages
        .iter()
        .map(|m| (m.role.clone(), m.text.as_str()))
        .collect();
    let [
        ..,
        (Role::Assistant, said),
        (Role::System, _),
        (Role::User, left),
    ] = texts.as_slice()
    else {
        panic!("expected reply, failure, then the steered message: {texts:?}");
    };
    assert_eq!((*said, *left), (answer.as_str(), steered.as_str()));
}

#[tokio::test]
async fn a_run_whose_start_was_not_saved_takes_no_messages_and_cannot_be_cancelled() {
    let api = ProviderApi::OpenAi;
    let mut rig = rig(api, vec![], None).await;
    // Another connection makes every snapshot write fail, as a full or
    // failing disk would; the committed snapshot stays readable.
    let db = rusqlite::Connection::open(rig._state.path().join("state.sqlite3")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER refuse BEFORE UPDATE ON snapshot BEGIN SELECT RAISE(ABORT, 'refused'); END;",
    )
    .unwrap();
    let Reply::Error(_) = rig.send().await.unwrap() else {
        panic!("a run whose start could not be saved must be refused");
    };
    assert_eq!(rig.thread().state, RunState::Idle);
    refused(rig.steer(rig.thread, &nonce()).await, ErrorKind::Conflict);
    refused(
        rig.run(Command::CancelRun { thread: rig.thread }).await,
        ErrorKind::NotFound,
    );
    // Once writes work again the conversation runs as usual.
    db.execute_batch("DROP TRIGGER refuse;").unwrap();
    let task = rig.send();
    rig.next_request()
        .await
        .respond(text_response(api, &nonce()))
        .await;
    assert_eq!(finished(task).await.state, RunState::Complete);
}

#[tokio::test]
async fn subagent_send_steers_a_running_subagent() {
    let api = ProviderApi::OpenAi;
    let mut rig = rig(api, vec![], None).await;
    for tool in [subagents::SPAWN, subagents::SEND] {
        rig.permit(tool, ToolPermission::Allow).await;
    }
    let task = rig.send();
    let first = rig.next_request().await;
    let spawn = Call::new(subagents::SPAWN, json!({"task": nonce()}));
    first
        .respond(calls_response(api, std::slice::from_ref(&spawn), &nonce()))
        .await;
    let (parent, child) = parent_and_child(&mut rig, api, &spawn.id).await;
    let id: Id = result_json(api, &parent.body, &spawn.id)["thread"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    // The subagent's model has not answered yet, so its run is going.
    let steered = nonce();
    let send = Call::new(subagents::SEND, json!({"thread": id, "message": steered}));
    parent
        .respond(calls_response(api, std::slice::from_ref(&send), &nonce()))
        .await;
    let third = rig.next_request().await;
    let sent = result_json(api, &third.body, &send.id);
    assert_eq!(sent["steered"], true, "{sent}");
    assert_eq!(thread_of(&rig, id).steering.len(), 1);
    child.respond(text_response(api, &nonce())).await;
    let again = rig.next_request().await;
    assert!(again.body.to_string().contains(&steered));
    again.respond(text_response(api, &nonce())).await;
    third.respond(text_response(api, &nonce())).await;
    assert_eq!(finished(task).await.state, RunState::Complete);
    tokio::time::timeout(PATIENCE, async {
        while thread_of(&rig, id).state != RunState::Complete {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(wait("the subagent to finish"));
    assert!(thread_of(&rig, id).steering.is_empty());
}

// ── Token budgets ───────────────────────────────────────────────────────────

impl Rig {
    async fn token_budget(&self, thread: Id, tokens: Option<u64>) -> Reply {
        self.run(Command::SetToolBudget {
            thread,
            budget: ToolBudget {
                tokens,
                ..ToolBudget::default()
            },
        })
        .await
    }
}

#[tokio::test]
async fn a_token_budget_stops_a_run_before_a_request_it_cannot_afford() {
    let api = ProviderApi::Anthropic;
    let name = nonce();
    let mut rig = rig(api, vec![tool(&name, ToolPermission::Allow, &[])], None).await;
    rig.permit(&name, ToolPermission::Allow).await;
    let (input, output) = (count(), count());
    // Room for the first request's usage, and nothing after the second's.
    let limit = input + output + 1;
    assert_eq!(rig.token_budget(rig.thread, Some(limit)).await, Reply::Ok);
    let task = rig.send();
    let first = rig.next_request().await;
    let call = Call::new(&name, json!({}));
    first
        .respond(with_usage(
            calls_response(api, std::slice::from_ref(&call), ""),
            input,
            output,
        ))
        .await;
    let second = rig.next_request().await;
    let late = Call::new(&name, json!({"value": nonce()}));
    second
        .respond(with_usage(
            calls_response(api, std::slice::from_ref(&late), ""),
            input,
            output,
        ))
        .await;
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Failed);
    let error = thread.error.clone().unwrap();
    assert_eq!(error.kind, ErrorKind::PermissionDenied);
    assert!(error.message.contains(&limit.to_string()), "{error:?}");
    // The first call ran; the one no request could follow was stopped.
    let calls = calls_of(&thread);
    assert_eq!(calls[0].state, ToolCallState::Succeeded);
    assert_ne!(calls[1].state, ToolCallState::Succeeded);
    assert_eq!(rig.executed().len(), 1);
}

#[tokio::test]
async fn a_token_budget_stops_a_run_whose_usage_is_unknown() {
    // These scripted OpenAI responses report no usage.
    let api = ProviderApi::OpenAi;
    let name = nonce();
    let mut rig = rig(api, vec![tool(&name, ToolPermission::Allow, &[])], None).await;
    rig.permit(&name, ToolPermission::Allow).await;
    // Without a limit the same run goes on.
    let task = rig.send();
    let first = rig.next_request().await;
    first
        .respond(calls_response(api, &[Call::new(&name, json!({}))], ""))
        .await;
    rig.next_request()
        .await
        .respond(text_response(api, &nonce()))
        .await;
    assert_eq!(finished(task).await.state, RunState::Complete);

    let limit = count() + 1_000_000;
    assert_eq!(rig.token_budget(rig.thread, Some(limit)).await, Reply::Ok);
    let task = rig.send();
    let first = rig.next_request().await;
    first
        .respond(calls_response(api, &[Call::new(&name, json!({}))], ""))
        .await;
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Failed);
    let error = thread.error.unwrap();
    assert_eq!(error.kind, ErrorKind::PermissionDenied);
    assert!(error.message.contains("did not report"), "{error:?}");

    // A final reply cannot be shown to have kept the limit either; the
    // reply itself is kept.
    let task = rig.send();
    let answer = nonce();
    rig.next_request()
        .await
        .respond(text_response(api, &answer))
        .await;
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Failed);
    assert!(thread.messages.iter().any(|m| m.text == answer));
    let error = thread.error.unwrap();
    assert!(error.message.contains("did not report"), "{error:?}");
}

#[tokio::test]
async fn a_token_budget_lowered_while_a_call_waits_stops_the_next_request() {
    let api = ProviderApi::Anthropic;
    let name = nonce();
    let mut rig = rig(api, vec![tool(&name, ToolPermission::Ask, &[])], None).await;
    rig.permit(&name, ToolPermission::Ask).await;
    let (input, output) = (count(), count());
    let limit = input + output + 1_000_000;
    assert_eq!(rig.token_budget(rig.thread, Some(limit)).await, Reply::Ok);
    let task = rig.send();
    let first = rig.next_request().await;
    first
        .respond(with_usage(
            calls_response(api, &[Call::new(&name, json!({}))], ""),
            input,
            output,
        ))
        .await;
    let approval = rig.pending_approval().await;
    // Lowered below what the run has already used while it waits.
    assert_eq!(rig.token_budget(rig.thread, Some(input)).await, Reply::Ok);
    assert_eq!(
        rig.resolve(&approval, &approval.digest, ApprovalDecision::Approve)
            .await,
        Reply::Ok
    );
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Failed);
    let error = thread.error.unwrap();
    assert_eq!(error.kind, ErrorKind::PermissionDenied);
    assert!(error.message.contains("token budget"), "{error:?}");
    assert!(rig.requests.try_recv().is_err());
}

#[tokio::test]
async fn a_subagents_token_budget_stays_within_its_parents() {
    let api = ProviderApi::OpenAi;
    let rig = rig(api, vec![], None).await;
    let parent = rig.thread;
    let Reply::Thread(child) = rig
        .run(Command::CreateSubagent {
            parent,
            title: nonce(),
            mode: Mode::Chat,
            model: rig.model.clone(),
            requested: Default::default(),
            profile: None,
        })
        .await
    else {
        panic!("subagent created");
    };
    let limit = count();
    assert_eq!(rig.token_budget(parent, Some(limit)).await, Reply::Ok);
    for denied in [None, Some(limit + 1)] {
        refused(
            rig.token_budget(child.id, denied).await,
            ErrorKind::PermissionDenied,
        );
    }
    assert_eq!(rig.token_budget(child.id, Some(limit)).await, Reply::Ok);
    refused(
        rig.token_budget(parent, Some(0)).await,
        ErrorKind::InvalidRequest,
    );
}

// ── Pausing ─────────────────────────────────────────────────────────────────

impl Rig {
    async fn pause(&self, thread: Id, paused: bool) -> Reply {
        self.run(if paused {
            Command::PauseRun { thread }
        } else {
            Command::ResumeRun { thread }
        })
        .await
    }
    /// Wait until `thread`'s run is waiting for a pause to end.
    async fn held(&self, thread: Id) {
        tokio::time::timeout(PATIENCE, async {
            while !thread_of(self, thread).held {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(wait("the run to be held"));
    }
}

#[tokio::test]
async fn a_paused_run_finishes_its_round_and_waits_until_resumed() {
    let api = ProviderApi::Anthropic;
    let name = nonce();
    let mut rig = rig(api, vec![tool(&name, ToolPermission::Allow, &[])], None).await;
    rig.permit(&name, ToolPermission::Allow).await;
    refused(rig.pause(rig.thread, true).await, ErrorKind::Conflict);
    let task = rig.send();
    let first = rig.next_request().await;
    assert_eq!(rig.pause(rig.thread, true).await, Reply::Ok);
    refused(rig.pause(rig.thread, true).await, ErrorKind::Conflict);
    // The round in progress, with its tool call, still finishes.
    let call = Call::new(&name, json!({"value": nonce()}));
    first
        .respond(calls_response(api, std::slice::from_ref(&call), ""))
        .await;
    rig.held(rig.thread).await;
    let thread = rig.thread();
    assert_eq!(calls_of(&thread)[0].state, ToolCallState::Succeeded);
    assert_eq!(thread.state, RunState::Running);
    // Held means no further request has been made.
    assert!(rig.requests.try_recv().is_err());

    assert_eq!(rig.pause(rig.thread, false).await, Reply::Ok);
    let second = rig.next_request().await;
    assert!(!rig.thread().held);
    second.respond(text_response(api, &nonce())).await;
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Complete, "{:?}", thread.error);
    assert!(!thread.paused && !thread.held);
    refused(rig.pause(rig.thread, false).await, ErrorKind::Conflict);
}

#[tokio::test]
async fn a_pause_made_while_a_round_is_prepared_holds_its_request() {
    let api = ProviderApi::Anthropic;
    let name = nonce();
    let mut rig = rig(api, vec![tool(&name, ToolPermission::Allow, &[])], None).await;
    rig.permit(&name, ToolPermission::Allow).await;
    let task = rig.send();
    let first = rig.next_request().await;
    // The next round's tool listing waits until released.
    let (entered, listing) = tokio::sync::oneshot::channel();
    let (release, waiting) = tokio::sync::oneshot::channel();
    *rig.listing.lock().unwrap() = Some(ListHold {
        entered,
        release: waiting,
    });
    first
        .respond(calls_response(api, &[Call::new(&name, json!({}))], ""))
        .await;
    tokio::time::timeout(PATIENCE, listing)
        .await
        .unwrap_or_else(wait("the next round's tool listing"))
        .unwrap();
    // The round has passed its first look; the pause still holds it.
    assert_eq!(rig.pause(rig.thread, true).await, Reply::Ok);
    release.send(()).unwrap();
    rig.held(rig.thread).await;
    assert!(rig.requests.try_recv().is_err());

    assert_eq!(rig.pause(rig.thread, false).await, Reply::Ok);
    rig.next_request()
        .await
        .respond(text_response(api, &nonce()))
        .await;
    assert_eq!(finished(task).await.state, RunState::Complete);
}

#[tokio::test]
async fn cancelling_a_paused_run_ends_it_and_clears_the_pause() {
    let api = ProviderApi::OpenAi;
    let name = nonce();
    let mut rig = rig(api, vec![tool(&name, ToolPermission::Allow, &[])], None).await;
    rig.permit(&name, ToolPermission::Allow).await;
    let task = rig.send();
    let first = rig.next_request().await;
    assert_eq!(rig.pause(rig.thread, true).await, Reply::Ok);
    first
        .respond(calls_response(api, &[Call::new(&name, json!({}))], ""))
        .await;
    rig.held(rig.thread).await;
    assert_eq!(
        rig.run(Command::CancelRun { thread: rig.thread }).await,
        Reply::Ok
    );
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Cancelled);
    assert!(!thread.paused && !thread.held);
    assert!(rig.requests.try_recv().is_err());
}

#[tokio::test]
async fn pausing_a_conversation_holds_its_subagents_until_resumed() {
    let api = ProviderApi::OpenAi;
    let mut rig = rig(api, vec![], None).await;
    rig.permit(subagents::SPAWN, ToolPermission::Allow).await;
    let task = rig.send();
    let first = rig.next_request().await;
    let spawn = Call::new(subagents::SPAWN, json!({"task": nonce()}));
    first
        .respond(calls_response(api, std::slice::from_ref(&spawn), &nonce()))
        .await;
    let (parent, child) = parent_and_child(&mut rig, api, &spawn.id).await;
    let id: Id = result_json(api, &parent.body, &spawn.id)["thread"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(rig.pause(rig.thread, true).await, Reply::Ok);
    // The subagent's own conversation is not paused; its parent is.
    refused(rig.pause(id, false).await, ErrorKind::Conflict);
    // A steered message would start another round, which the pause holds.
    let steered = nonce();
    assert_eq!(rig.steer(id, &steered).await, Reply::Ok);
    child.respond(text_response(api, &nonce())).await;
    rig.held(id).await;
    assert!(rig.requests.try_recv().is_err());

    assert_eq!(rig.pause(rig.thread, false).await, Reply::Ok);
    let again = rig.next_request().await;
    assert!(again.body.to_string().contains(&steered));
    again.respond(text_response(api, &nonce())).await;
    parent.respond(text_response(api, &nonce())).await;
    assert_eq!(finished(task).await.state, RunState::Complete);
    assert!(!thread_of(&rig, id).held);
}
