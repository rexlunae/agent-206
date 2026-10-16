//! The native run engine's tool loop against scripted provider streams and a
//! test-only tool host. Every identifier, argument and result is generated at
//! runtime.
use a206_core::*;
use a206_runtime::{
    gateway::Gateway,
    tools::{ToolHost, ToolListing, ToolOutput, ToolRequest, ToolScope, ToolSpec},
};
use axum::{Json, Router, extract::State, http::Method};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;

const PROTOCOLS: [ProviderApi; 3] = [
    ProviderApi::OpenAi,
    ProviderApi::Anthropic,
    ProviderApi::Gemini,
];
fn nonce() -> String {
    Id::new_v4().simple().to_string()
}
fn wait<T>(what: &'static str) -> impl FnOnce(tokio::time::error::Elapsed) -> T {
    move |_| panic!("timed out waiting for {what}")
}
const PATIENCE: Duration = Duration::from_secs(10);

// ── Scripted provider ───────────────────────────────────────────────────────

/// One streamed request the test answers by writing its body.
struct Opened {
    body: Value,
    feed: mpsc::Sender<Vec<u8>>,
}
impl Opened {
    async fn respond(self, events: String) {
        self.feed.send(events.into_bytes()).await.unwrap();
        // Dropping the feed ends the response body.
    }
}
#[derive(Clone)]
struct Scripted {
    opened: mpsc::Sender<Opened>,
    catalog: Value,
}
async fn handle(
    State(api): State<Scripted>,
    method: Method,
    body: axum::body::Bytes,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if method == Method::GET {
        return Json(api.catalog).into_response();
    }
    let (feed, pieces) = mpsc::channel::<Vec<u8>>(8);
    let opened = Opened {
        body: serde_json::from_slice(&body).unwrap(),
        feed,
    };
    api.opened.send(opened).await.unwrap();
    let body = futures_util::stream::unfold(pieces, |mut pieces| async move {
        let piece = pieces.recv().await?;
        Some((Ok::<_, std::io::Error>(piece), pieces))
    });
    (
        [("content-type", "text/event-stream")],
        axum::body::Body::from_stream(body),
    )
        .into_response()
}
fn sse(name: Option<&str>, value: Value) -> String {
    let name = name.map(|n| format!("event: {n}\n")).unwrap_or_default();
    format!("{name}data: {value}\n\n")
}
/// A complete response carrying only text.
fn text_response(api: ProviderApi, text: &str) -> String {
    match api {
        ProviderApi::OpenAi => [
            sse(None, json!({"choices": [{"index": 0, "delta": {"content": text}}]})),
            sse(None, json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})),
            "data: [DONE]\n\n".into(),
        ]
        .concat(),
        ProviderApi::Anthropic => [
            sse(Some("message_start"), json!({"type": "message_start", "message": {"usage": {"input_tokens": 1}}})),
            sse(Some("content_block_delta"), json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}})),
            sse(Some("message_delta"), json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}})),
            sse(Some("message_stop"), json!({"type": "message_stop"})),
        ]
        .concat(),
        ProviderApi::Gemini => sse(
            None,
            json!({"candidates": [{"content": {"parts": [{"text": text}]}, "finishReason": "STOP"}]}),
        ),
    }
}
/// A tool call as the model sends it: ID, tool name, raw argument text.
#[derive(Clone)]
struct Call {
    id: String,
    tool: String,
    arguments: String,
}
impl Call {
    fn new(tool: &str, arguments: Value) -> Self {
        Self {
            id: format!("call_{}", nonce()),
            tool: tool.into(),
            arguments: arguments.to_string(),
        }
    }
    fn raw(tool: &str, arguments: &str) -> Self {
        Self {
            id: format!("call_{}", nonce()),
            tool: tool.into(),
            arguments: arguments.into(),
        }
    }
}
/// A complete response requesting `calls`, with each call's arguments split
/// across deltas where the protocol streams them. Gemini parts carry
/// `signature` as their thought signature.
fn calls_response(api: ProviderApi, calls: &[Call], signature: &str) -> String {
    let split = |text: &str| {
        let mut middle = text.len() / 2;
        while !text.is_char_boundary(middle) {
            middle -= 1;
        }
        (text[..middle].to_owned(), text[middle..].to_owned())
    };
    match api {
        ProviderApi::OpenAi => {
            let mut events = vec![sse(
                None,
                json!({"choices": [{"index": 0, "delta": {"role": "assistant"}}]}),
            )];
            for (index, call) in calls.iter().enumerate() {
                let (head, tail) = split(&call.arguments);
                events.push(sse(None, json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": index, "id": call.id, "type": "function", "function": {"name": call.tool, "arguments": ""}}]}}]})));
                for piece in [head, tail] {
                    events.push(sse(None, json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": index, "function": {"arguments": piece}}]}}]})));
                }
            }
            events.push(sse(
                None,
                json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
            ));
            events.push("data: [DONE]\n\n".into());
            events.concat()
        }
        ProviderApi::Anthropic => {
            let mut events = vec![sse(
                Some("message_start"),
                json!({"type": "message_start", "message": {"usage": {"input_tokens": 1}}}),
            )];
            for (index, call) in calls.iter().enumerate() {
                let (head, tail) = split(&call.arguments);
                events.push(sse(Some("content_block_start"), json!({"type": "content_block_start", "index": index, "content_block": {"type": "tool_use", "id": call.id, "name": call.tool, "input": {}}})));
                for piece in [head, tail] {
                    events.push(sse(Some("content_block_delta"), json!({"type": "content_block_delta", "index": index, "delta": {"type": "input_json_delta", "partial_json": piece}})));
                }
                events.push(sse(
                    Some("content_block_stop"),
                    json!({"type": "content_block_stop", "index": index}),
                ));
            }
            events.push(sse(Some("message_delta"), json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 1}})));
            events.push(sse(Some("message_stop"), json!({"type": "message_stop"})));
            events.concat()
        }
        ProviderApi::Gemini => {
            let parts: Vec<_> = calls
                .iter()
                .map(|call| {
                    let args: Value = serde_json::from_str(&call.arguments).unwrap();
                    json!({"functionCall": {"id": call.id, "name": call.tool, "args": args}, "thoughtSignature": signature})
                })
                .collect();
            sse(
                None,
                json!({"candidates": [{"content": {"role": "model", "parts": parts}, "finishReason": "STOP"}]}),
            )
        }
    }
}
/// The tool names a request offered, whatever its protocol.
fn offered(api: ProviderApi, body: &Value) -> Vec<String> {
    let names = match api {
        ProviderApi::OpenAi => body["tools"].as_array().map(|tools| {
            tools
                .iter()
                .map(|t| t["function"]["name"].clone())
                .collect::<Vec<_>>()
        }),
        ProviderApi::Anthropic => body["tools"]
            .as_array()
            .map(|tools| tools.iter().map(|t| t["name"].clone()).collect()),
        ProviderApi::Gemini => body["tools"][0]["functionDeclarations"]
            .as_array()
            .map(|tools| tools.iter().map(|t| t["name"].clone()).collect()),
    };
    names
        .unwrap_or_default()
        .into_iter()
        .map(|name| name.as_str().unwrap().to_owned())
        .collect()
}
/// The result text a request carries for `call`, whatever its protocol.
fn result_for(api: ProviderApi, body: &Value, call: &str) -> Option<String> {
    let messages = match api {
        ProviderApi::Gemini => body["contents"].as_array()?,
        _ => body["messages"].as_array()?,
    };
    messages.iter().find_map(|message| match api {
        ProviderApi::OpenAi => (message["role"] == "tool" && message["tool_call_id"] == call)
            .then(|| message["content"].as_str().unwrap().to_owned()),
        ProviderApi::Anthropic => message["content"].as_array()?.iter().find_map(|block| {
            (block["type"] == "tool_result" && block["tool_use_id"] == call)
                .then(|| block["content"].as_str().unwrap().to_owned())
        }),
        ProviderApi::Gemini => message["parts"].as_array()?.iter().find_map(|part| {
            let response = &part["functionResponse"];
            (response["id"] == call).then(|| {
                let result = &response["response"];
                result["output"]
                    .as_str()
                    .or(result["error"].as_str())
                    .unwrap()
                    .to_owned()
            })
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
}
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
            let text = format!(
                "{}:{}",
                self.marker,
                Value::Object(request.arguments.clone())
            );
            self.calls.send((scope.clone(), request)).unwrap();
            if blocking {
                let _flag = DropFlag(self.dropped.clone());
                std::future::pending::<()>().await;
            }
            Ok(ToolOutput {
                text,
                is_error: false,
            })
        })
    }
}
fn tool(name: &str, permission: ToolPermission, capabilities: &[Capability]) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: nonce(),
        parameters: json!({"type": "object", "properties": {}}),
        capabilities: capabilities.iter().copied().collect(),
        permission,
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
}
async fn rig(api: ProviderApi, tools: Vec<ToolSpec>, blocking: Option<String>) -> Rig {
    let model = nonce();
    let catalog = match api {
        ProviderApi::Gemini => {
            json!({"models": [{"name": format!("models/{model}"), "supportedGenerationMethods": ["generateContent"]}]})
        }
        _ => json!({"data": [{"id": model}]}),
    };
    let (opened, requests) = mpsc::channel(8);
    let router = Router::new()
        .fallback(handle)
        .with_state(Scripted { opened, catalog });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    // Detached: the server lives as long as the test's runtime.
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let (sender, calls) = mpsc::unbounded_channel();
    let marker = nonce();
    let dropped = Arc::new(AtomicBool::new(false));
    let state = tempfile::tempdir().unwrap();
    let gateway = Gateway::open(state.path())
        .unwrap()
        .with_tool_host(Arc::new(TestHost {
            tools,
            marker: marker.clone(),
            blocking,
            calls: sender,
            dropped: dropped.clone(),
        }));
    let provider = Id::new_v4();
    let mut settings = Settings::default();
    settings.providers.push(ProviderConfig {
        id: provider,
        label: nonce(),
        base_url: format!("http://{address}/v1"),
        credential: None,
        api,
    });
    let mut rig = Rig {
        gateway,
        _state: state,
        thread: Id::nil(),
        model: ModelKey {
            provider,
            model: String::new(),
        },
        requests,
        calls,
        marker,
        dropped,
    };
    assert_eq!(rig.run(Command::SaveSettings(settings)).await, Reply::Ok);
    let Reply::Catalog(catalog) = rig.run(Command::RefreshCatalog { provider }).await else {
        panic!("catalog refresh failed");
    };
    rig.model = catalog.models[0].key.clone();
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
        self.gateway
            .request(a206_protocol::Request {
                id: Id::new_v4(),
                body,
            })
            .await
            .body
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
        let gateway = self.gateway.clone();
        let thread = self.thread;
        tokio::spawn(async move {
            gateway
                .request(a206_protocol::Request {
                    id: Id::new_v4(),
                    body: Command::Send {
                        thread,
                        text: nonce(),
                        attachments: vec![],
                    },
                })
                .await
                .body
        })
    }
    async fn next_request(&mut self) -> Opened {
        tokio::time::timeout(PATIENCE, self.requests.recv())
            .await
            .unwrap_or_else(wait("a provider request"))
            .unwrap()
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
async fn finished(task: tokio::task::JoinHandle<Reply>) -> Thread {
    match tokio::time::timeout(PATIENCE, task)
        .await
        .unwrap_or_else(wait("the run to finish"))
        .unwrap()
    {
        Reply::Thread(thread) => *thread,
        other => panic!("run reply: {other:?}"),
    }
}
/// The tool calls of the run's assistant messages, in order.
fn calls_of(thread: &Thread) -> Vec<ToolCall> {
    thread
        .messages
        .iter()
        .flat_map(|m| m.tool_calls.clone())
        .collect()
}
fn error_kind(call: &ToolCall) -> ErrorKind {
    call.error
        .as_ref()
        .expect("a refused call carries a report")
        .kind
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
    assert_eq!(list.tools[0].effective, ToolPermission::Ask);
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
    assert_eq!(list.tools[0].effective, ToolPermission::Deny);
}
