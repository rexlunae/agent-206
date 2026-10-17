//! Scripted provider streams and gateway helpers shared by the run-engine
//! tests. Every identifier, argument and result is generated at runtime.
// Each test binary uses its own subset of these helpers.
#![allow(dead_code)]
use a206_core::*;
use a206_runtime::gateway::Gateway;
use axum::{Json, Router, extract::State, http::Method};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::mpsc;

pub const PROTOCOLS: [ProviderApi; 3] = [
    ProviderApi::OpenAi,
    ProviderApi::Anthropic,
    ProviderApi::Gemini,
];
pub fn nonce() -> String {
    Id::new_v4().simple().to_string()
}
pub fn wait<T>(what: &'static str) -> impl FnOnce(tokio::time::error::Elapsed) -> T {
    move |_| panic!("timed out waiting for {what}")
}
pub const PATIENCE: Duration = Duration::from_secs(10);

// ── Scripted provider ───────────────────────────────────────────────────────

/// One streamed request the test answers by writing its body.
pub struct Opened {
    pub body: Value,
    pub feed: mpsc::Sender<Vec<u8>>,
}
impl Opened {
    pub async fn respond(self, events: String) {
        self.feed.send(events.into_bytes()).await.unwrap();
        // Dropping the feed ends the response body.
    }
}
#[derive(Clone)]
pub struct Scripted {
    opened: mpsc::Sender<Opened>,
    catalog: Value,
}
pub async fn handle(
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
pub fn sse(name: Option<&str>, value: Value) -> String {
    let name = name.map(|n| format!("event: {n}\n")).unwrap_or_default();
    format!("{name}data: {value}\n\n")
}
/// A complete response carrying only text.
pub fn text_response(api: ProviderApi, text: &str) -> String {
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
/// A complete text response whose usage reports `input` input tokens.
pub fn text_response_reporting(api: ProviderApi, text: &str, input: u64) -> String {
    match api {
        ProviderApi::OpenAi => [
            sse(
                None,
                json!({"choices": [{"index": 0, "delta": {"content": text}}]}),
            ),
            sse(
                None,
                json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
            ),
            sse(
                None,
                json!({"choices": [], "usage": {"prompt_tokens": input, "completion_tokens": 1}}),
            ),
            "data: [DONE]\n\n".into(),
        ]
        .concat(),
        ProviderApi::Anthropic => text_response(api, text)
            .replace("\"input_tokens\":1", &format!("\"input_tokens\":{input}")),
        ProviderApi::Gemini => sse(
            None,
            json!({"candidates": [{"content": {"parts": [{"text": text}]}, "finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": input, "candidatesTokenCount": 1}}),
        ),
    }
}
/// The messages (or Gemini contents) a request carries.
pub fn history_of(api: ProviderApi, body: &Value) -> Vec<Value> {
    match api {
        ProviderApi::Gemini => body["contents"].as_array().unwrap().clone(),
        _ => body["messages"].as_array().unwrap().clone(),
    }
}
/// The system prompt a request carries.
pub fn system_of(api: ProviderApi, body: &Value) -> String {
    match api {
        ProviderApi::OpenAi => body["messages"][0]["content"].as_str().unwrap().to_owned(),
        ProviderApi::Anthropic => body["system"].as_str().unwrap().to_owned(),
        ProviderApi::Gemini => body["systemInstruction"]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned(),
    }
}
/// A tool call as the model sends it: ID, tool name, raw argument text.
#[derive(Clone)]
pub struct Call {
    pub id: String,
    pub tool: String,
    pub arguments: String,
}
impl Call {
    pub fn new(tool: &str, arguments: Value) -> Self {
        Self {
            id: format!("call_{}", nonce()),
            tool: tool.into(),
            arguments: arguments.to_string(),
        }
    }
    pub fn raw(tool: &str, arguments: &str) -> Self {
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
pub fn calls_response(api: ProviderApi, calls: &[Call], signature: &str) -> String {
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
pub fn offered(api: ProviderApi, body: &Value) -> Vec<String> {
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
pub fn result_for(api: ProviderApi, body: &Value, call: &str) -> Option<String> {
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

pub async fn finished(task: tokio::task::JoinHandle<Reply>) -> Thread {
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
pub fn calls_of(thread: &Thread) -> Vec<ToolCall> {
    thread
        .messages
        .iter()
        .flat_map(|m| m.tool_calls.clone())
        .collect()
}
pub fn error_kind(call: &ToolCall) -> ErrorKind {
    call.error
        .as_ref()
        .expect("a refused call carries a report")
        .kind
}

/// A scripted provider of `api` serving one generated model, configured in
/// `gateway`. Returns the model and the requests the provider receives.
pub async fn connect(gateway: &Gateway, api: ProviderApi) -> (ModelKey, mpsc::Receiver<Opened>) {
    connect_reporting(gateway, api, None).await
}
/// Like [`connect`], with the models API reporting `context_limit` (in the
/// protocol's own field) when given.
pub async fn connect_reporting(
    gateway: &Gateway,
    api: ProviderApi,
    context_limit: Option<u64>,
) -> (ModelKey, mpsc::Receiver<Opened>) {
    let model = nonce();
    let mut item = match api {
        ProviderApi::Gemini => {
            json!({"name": format!("models/{model}"), "supportedGenerationMethods": ["generateContent"]})
        }
        _ => json!({"id": model}),
    };
    if let Some(limit) = context_limit {
        let field = match api {
            ProviderApi::OpenAi => "context_length",
            ProviderApi::Anthropic => "max_input_tokens",
            ProviderApi::Gemini => "inputTokenLimit",
        };
        item[field] = json!(limit);
    }
    let catalog = match api {
        ProviderApi::Gemini => json!({"models": [item]}),
        _ => json!({"data": [item]}),
    };
    let (opened, requests) = mpsc::channel(8);
    let router = Router::new()
        .fallback(handle)
        // Requests near the conversation's storage limit are several MiB.
        .layer(axum::extract::DefaultBodyLimit::disable())
        .with_state(Scripted { opened, catalog });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    // Detached: the server lives as long as the test's runtime.
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let provider = Id::new_v4();
    let mut settings = Settings::default();
    settings.providers.push(ProviderConfig {
        id: provider,
        label: nonce(),
        base_url: format!("http://{address}/v1"),
        credential: None,
        api,
    });
    assert_eq!(
        request(gateway, Command::SaveSettings(settings)).await,
        Reply::Ok
    );
    let Reply::Catalog(catalog) = request(gateway, Command::RefreshCatalog { provider }).await
    else {
        panic!("catalog refresh failed");
    };
    (catalog.models[0].key.clone(), requests)
}
pub async fn request(gateway: &Gateway, body: Command) -> Reply {
    gateway
        .request(a206_protocol::Request {
            id: Id::new_v4(),
            body,
        })
        .await
        .body
}
/// Start a run in the background; its reply is the finished thread.
pub fn send(gateway: &Gateway, thread: Id) -> tokio::task::JoinHandle<Reply> {
    let gateway = gateway.clone();
    tokio::spawn(async move {
        request(
            &gateway,
            Command::Send {
                thread,
                text: nonce(),
                attachments: vec![],
            },
        )
        .await
    })
}
pub async fn next_request(requests: &mut mpsc::Receiver<Opened>) -> Opened {
    tokio::time::timeout(PATIENCE, requests.recv())
        .await
        .unwrap_or_else(wait("a provider request"))
        .unwrap()
}
