//! Native protocol contracts against a live API with generated model facts.
use a206_core::*;
use a206_runtime::{backends::Progress, provider, retry::RetryPolicy};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::HeaderMap,
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
#[derive(Clone)]
struct Api {
    id: String,
    next: String,
    cursor: String,
    limit: u64,
    output: u64,
    effort: String,
    text: String,
    api: ProviderApi,
    key: String,
    requests: Arc<Mutex<Vec<Value>>>,
}
/// The credential must arrive in the protocol's own header, attached by the broker.
fn assert_key(api: &Api, headers: &HeaderMap) {
    let header = match api.api {
        ProviderApi::Anthropic => "x-api-key",
        _ => "x-goog-api-key",
    };
    assert_eq!(headers.get(header).unwrap(), api.key.as_str());
    assert!(headers.get("authorization").is_none());
}
async fn start_broker(
    origin: &str,
    key: &str,
) -> (tempfile::TempDir, String, a206_core::BrokerPairing) {
    use a206_broker::*;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_owned();
    let serving = root.clone();
    tokio::spawn(async move { serve(&serving).await.unwrap() });
    while !socket_path(&root).exists() {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let mut user = Client::connect(&root, Role::User).await.unwrap();
    user.call(Request::Unlock {
        password: WireSecret::new(Id::new_v4().to_string()),
        totp: None,
        create: true,
    })
    .await
    .unwrap();
    let name = Id::new_v4().to_string();
    user.call(Request::PutCredential {
        name: name.clone(),
        meta: a206_vault::CredentialMeta {
            label: name.clone(),
            kind: a206_vault::CredentialKind::ApiKey,
            policy: a206_vault::AccessPolicy::WithApproval,
            description: None,
            disabled: false,
            destinations: vec![origin.to_owned()],
        },
        value: WireValue::Single(WireSecret::new(key.to_owned())),
        replace: false,
    })
    .await
    .unwrap();
    let Reply::GatewayPairing { token } = user.call(Request::PairGateway).await.unwrap() else {
        panic!("expected a pairing");
    };
    (
        dir,
        name,
        a206_core::BrokerPairing::new(token.expose().to_owned()),
    )
}
async fn models(
    State(api): State<Api>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Json<Value> {
    let cursor = if api.api == ProviderApi::Gemini {
        "pageToken"
    } else {
        "after_id"
    };
    assert_key(&api, &headers);
    let second = query.get(cursor) == Some(&api.cursor);
    let id = if second { &api.next } else { &api.id };
    match api.api {
        ProviderApi::Anthropic => {
            assert_eq!(headers.get("anthropic-version").unwrap(), "2023-06-01");
            Json(
                json!({"data": [{"id": id, "display_name": id, "max_input_tokens": api.limit, "max_tokens": api.output,
                "capabilities": {"thinking": {"supported": true}, "effort": {api.effort.clone(): {"supported": true}, "supported": true}}}], "has_more": !second, "last_id": api.cursor}),
            )
        }
        ProviderApi::Gemini => {
            let mut value = json!({"models": [{"name": format!("models/{id}"), "displayName": id, "inputTokenLimit": api.limit, "outputTokenLimit": api.output,
                "supportedGenerationMethods": ["generateContent"], "thinking": true}, {"name": Id::new_v4().to_string(), "supportedGenerationMethods": ["embedContent"]}]});
            if !second {
                value["nextPageToken"] = json!(api.cursor);
            }
            Json(value)
        }
        _ => unreachable!(),
    }
}
/// A server-sent event whose data is `value`.
fn event(name: Option<&str>, value: Value) -> String {
    let name = name.map(|n| format!("event: {n}\n")).unwrap_or_default();
    format!("{name}data: {value}\n\n")
}
fn sse(body: String) -> impl axum::response::IntoResponse {
    ([("content-type", "text/event-stream")], body)
}
/// Two text deltas around a thinking delta, which must not reach the reply.
async fn messages(
    State(api): State<Api>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl axum::response::IntoResponse {
    assert_key(&api, &headers);
    assert_eq!(headers.get("anthropic-version").unwrap(), "2023-06-01");
    api.requests.lock().unwrap().push(body);
    let (head, tail) = api.text.split_at(api.text.len() / 2);
    sse([
        event(Some("message_start"), json!({"type": "message_start", "message": {"usage": {"input_tokens": api.limit, "output_tokens": 1}}})),
        event(Some("content_block_delta"), json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": Id::new_v4().to_string()}})),
        event(Some("content_block_delta"), json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": head}})),
        event(Some("ping"), json!({"type": "ping"})),
        event(Some("content_block_delta"), json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": tail}})),
        event(Some("message_delta"), json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": api.output}})),
        event(Some("message_stop"), json!({"type": "message_stop"})),
    ].concat())
}
async fn generate(
    State(api): State<Api>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl axum::response::IntoResponse {
    assert_key(&api, &headers);
    assert_eq!(query.get("alt").map(String::as_str), Some("sse"));
    api.requests.lock().unwrap().push(body);
    let (head, tail) = api.text.split_at(api.text.len() / 2);
    sse([
        event(None, json!({"candidates": [{"content": {"parts": [{"thought": true, "text": Id::new_v4().to_string()}, {"text": head}]}}]})),
        event(None, json!({"candidates": [{"content": {"parts": [{"text": tail}]}, "finishReason": "STOP"}], "usageMetadata": {"promptTokenCount": api.limit, "candidatesTokenCount": api.output}})),
    ].concat())
}
#[tokio::test]
async fn paginates_native_catalogs_and_dispatches_protocol_specific_text() {
    for protocol in [ProviderApi::Anthropic, ProviderApi::Gemini] {
        let identity = Id::new_v4();
        let api = Api {
            id: identity.to_string(),
            next: Id::new_v4().to_string(),
            cursor: format!("{}&next=/other", Id::new_v4()),
            limit: (identity.as_u128() % 100_000 + 1) as u64,
            output: (identity.as_u128() % 20_000 + 1) as u64,
            effort: Id::new_v4().to_string(),
            text: Id::new_v4().to_string(),
            api: protocol,
            key: Id::new_v4().to_string(),
            requests: Default::default(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new()
            .route("/v1/models", get(models))
            .route("/v1/messages", post(messages))
            .route(
                &format!("/v1/models/{}:streamGenerateContent", api.id),
                post(generate),
            )
            .with_state(api.clone());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let (broker, credential, pairing) =
            start_broker(&format!("http://{address}"), &api.key).await;
        let config = ProviderConfig {
            id: identity,
            label: Id::new_v4().to_string(),
            base_url: format!("http://{address}/v1"),
            credential: Some(credential),
            api: protocol,
        };
        // Without the pairing a trusted client minted, the gateway is refused.
        let unpaired = provider::Broker {
            root: broker.path(),
            pairing: None,
        };
        assert!(matches!(
            provider::catalog(&config, unpaired).await,
            Err(provider::ProviderError::NotPaired)
        ));
        let forged = a206_core::BrokerPairing::new(Id::new_v4().to_string());
        let forged = provider::Broker {
            root: broker.path(),
            pairing: Some(&forged),
        };
        assert!(provider::catalog(&config, forged).await.is_err());
        // As the gateway does, keep the connection that claims the pairing open.
        let _anchor = provider::verify_pairing(broker.path(), &pairing)
            .await
            .unwrap();
        let broker = provider::Broker {
            root: broker.path(),
            pairing: Some(&pairing),
        };
        let catalog = provider::catalog(&config, broker).await.unwrap();
        assert_eq!(catalog.models.len(), 2);
        let model = &catalog.models[0];
        assert_eq!(model.context_limit, Some(api.limit));
        assert_eq!(model.output_limit, Some(api.output));
        assert!(model.input_cost_per_token.is_none() && catalog.balance.is_none());
        let mut thread = Thread::new(Id::new_v4().to_string(), Mode::Chat);
        thread.model = Some(model.key.clone());
        thread.messages.push(Message {
            id: Id::new_v4(),
            role: Role::User,
            text: api.text.clone(),
            attachments: vec![],
            images: vec![],
            at: provider::now(),
            backend: CodingBackend::Native,
            tool_calls: vec![],
            compaction: None,
            reasoning: String::new(),
            reasoning_shortened: false,
        });
        if protocol == ProviderApi::Anthropic {
            thread.effort = Some(api.effort.clone());
            assert_eq!(model.reasoning_efforts, vec![api.effort.clone()]);
        }
        let prompt = Id::new_v4().to_string();
        let (progress, mut deltas) = tokio::sync::mpsc::channel(64);
        let provider::Turn { text, usage, .. } = provider::complete(
            &config,
            broker,
            provider::Ask {
                thread: &thread,
                prompt: &prompt,
                info: model,
                tools: &[],
                images: &provider::ImageData::default(),
            },
            &progress,
            &RetryPolicy::NONE,
        )
        .await
        .unwrap();
        assert_eq!(text, api.text);
        let (streamed, ..) = drain(&mut deltas);
        assert_eq!(streamed, api.text);
        assert_eq!(usage.input_tokens, Some(api.limit));
        assert_eq!(usage.output_tokens, Some(api.output));
        let request = api.requests.lock().unwrap()[0].clone();
        match protocol {
            ProviderApi::Anthropic => {
                assert_eq!(request["stream"], true);
                assert_eq!(request["system"], prompt);
                assert_eq!(request["max_tokens"], api.output.min(8192));
                assert_eq!(request["output_config"]["effort"], api.effort);
            }
            ProviderApi::Gemini => {
                assert_eq!(request["systemInstruction"]["parts"][0]["text"], prompt);
                assert_eq!(request["contents"][0]["parts"][0]["text"], api.text);
            }
            _ => unreachable!(),
        }
        task.abort();
    }
}
#[tokio::test]
async fn repeated_pagination_is_rejected_and_credentials_never_bypass_the_broker() {
    let id = Id::new_v4().to_string();
    let cursor = Id::new_v4().to_string();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = Router::new().route(
        "/v1/models",
        get(move || {
            let id = id.clone();
            let cursor = cursor.clone();
            async move { Json(json!({"data": [{"id": id}], "has_more": true, "last_id": cursor})) }
        }),
    );
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut config = ProviderConfig {
        id: Id::new_v4(),
        label: Id::new_v4().to_string(),
        base_url: format!("http://{address}/v1"),
        credential: None,
        api: ProviderApi::Anthropic,
    };
    let absent = tempfile::tempdir().unwrap();
    let broker = provider::Broker {
        root: absent.path(),
        pairing: None,
    };
    assert!(provider::catalog(&config, broker).await.is_err());
    // Without a running, unlocked broker a credential is never usable.
    config.credential = Some(Id::new_v4().to_string());
    assert!(
        provider::catalog(&config, broker)
            .await
            .unwrap_err()
            .to_string()
            .contains("unlock the vault")
    );
    assert!(provider::endpoint(&config, "../outside").is_err());
    assert!(provider::endpoint(&config, "https://example.org/").is_err());
    task.abort();
}

#[tokio::test]
async fn reads_compatible_context_window_and_nested_effort_metadata_in_api_order() {
    let identity = Id::new_v4();
    let context = (identity.as_u128() % 100_000 + 1) as u64;
    let output = (identity.as_u128() % context as u128 + 1) as u64;
    let model = identity.to_string();
    let efforts = vec![Id::new_v4().to_string(), Id::new_v4().to_string()];
    let payload = json!({"data": [{"id": model, "context_window": context, "max_output_tokens": output,
        "effort": {"supported_levels": [efforts[0], efforts[1], efforts[0]]}}]});
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = Router::new().route(
        "/models",
        get(move || {
            let payload = payload.clone();
            async move { Json(payload) }
        }),
    );
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let config = ProviderConfig {
        id: Id::new_v4(),
        label: Id::new_v4().to_string(),
        base_url: format!("http://{address}"),
        credential: None,
        api: ProviderApi::OpenAi,
    };
    let none = tempfile::tempdir().unwrap();
    let broker = provider::Broker {
        root: none.path(),
        pairing: None,
    };
    let catalog = provider::catalog(&config, broker).await.unwrap();
    assert_eq!(catalog.models[0].key.model, model);
    assert_eq!(catalog.models[0].context_limit, Some(context));
    assert_eq!(catalog.models[0].output_limit, Some(output));
    assert_eq!(catalog.models[0].reasoning_efforts, efforts);
    assert!(catalog.models[0].input_cost_per_token.is_none());
    task.abort();
}
/// The gateway only spends a credential after a trusted client hands it this
/// unlock's pairing, and the pairing is never written into the profile.
#[tokio::test]
async fn gateway_needs_a_verified_pairing_and_never_persists_it() {
    let identity = Id::new_v4();
    let api = Api {
        id: identity.to_string(),
        next: Id::new_v4().to_string(),
        cursor: Id::new_v4().to_string(),
        limit: (identity.as_u128() % 100_000 + 1) as u64,
        output: (identity.as_u128() % 20_000 + 1) as u64,
        effort: Id::new_v4().to_string(),
        text: Id::new_v4().to_string(),
        api: ProviderApi::Anthropic,
        key: Id::new_v4().to_string(),
        requests: Default::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = Router::new()
        .route("/v1/models", get(models))
        .with_state(api.clone());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let (dir, credential, pairing) = start_broker(&format!("http://{address}"), &api.key).await;
    let gateway = a206_runtime::gateway::Gateway::open(dir.path()).unwrap();
    let run = |body| {
        let gateway = gateway.clone();
        async move {
            gateway
                .request(a206_protocol::Request {
                    id: Id::new_v4(),
                    body,
                })
                .await
                .body
        }
    };
    let provider = Id::new_v4();
    let mut settings = Settings::default();
    settings.providers.push(ProviderConfig {
        id: provider,
        label: Id::new_v4().to_string(),
        base_url: format!("http://{address}/v1"),
        credential: Some(credential),
        api: ProviderApi::Anthropic,
    });
    assert_eq!(
        run(Command::SaveSettings(Box::new(settings))).await,
        Reply::Ok
    );
    let refresh = || Command::RefreshCatalog { provider };
    assert!(matches!(
        run(refresh()).await,
        Reply::Error(report)
            if report.kind == a206_core::ErrorKind::Unavailable
                && report.message.contains("not paired")
    ));
    // Automatic discovery fails too, and is throttled until something changes.
    gateway.refresh_stale_catalogs().await.unwrap();
    assert!(gateway.snapshot().unwrap().catalogs.is_empty());
    // A pairing the broker never issued is refused and not kept.
    let forged = BrokerPairing::new(Id::new_v4().to_string());
    assert!(matches!(
        run(Command::PairBroker { pairing: forged }).await,
        Reply::Error { .. }
    ));
    assert!(matches!(run(refresh()).await, Reply::Error { .. }));
    assert_eq!(
        run(Command::PairBroker {
            pairing: pairing.clone()
        })
        .await,
        Reply::Ok
    );
    // A verified pairing lifts the throttle, so discovery need not wait it out.
    gateway.refresh_stale_catalogs().await.unwrap();
    let catalogs = gateway.snapshot().unwrap().catalogs;
    assert_eq!(catalogs.len(), 1);
    assert_eq!(catalogs[0].models[0].key.model, api.id);
    // A retried delivery of the same pairing must not drop the connection
    // that anchors it at the broker.
    assert_eq!(
        run(Command::PairBroker {
            pairing: pairing.clone()
        })
        .await,
        Reply::Ok
    );
    // A dropped anchor is noticed by the broker asynchronously; keep using the
    // pairing for a while to make sure it stays valid.
    for _ in 0..20 {
        assert!(matches!(run(refresh()).await, Reply::Catalog(_)));
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let Reply::Catalog(catalog) = run(refresh()).await else {
        panic!("paired refresh failed");
    };
    assert_eq!(catalog.models[0].key.model, api.id);
    // A retry that cannot reach the broker is reported but keeps the pairing.
    let socket = a206_broker::socket_path(dir.path());
    let aside = socket.with_extension("aside");
    std::fs::rename(&socket, &aside).unwrap();
    assert!(matches!(
        run(Command::PairBroker {
            pairing: pairing.clone()
        })
        .await,
        Reply::Error { .. }
    ));
    std::fs::rename(&aside, &socket).unwrap();
    assert!(matches!(run(refresh()).await, Reply::Catalog(_)));
    // Once the broker voids the pairing, a retry is no longer acknowledged.
    let mut user = a206_broker::Client::connect(dir.path(), a206_broker::Role::User)
        .await
        .unwrap();
    user.call(a206_broker::Request::Lock).await.unwrap();
    assert!(matches!(
        run(Command::PairBroker {
            pairing: pairing.clone()
        })
        .await,
        Reply::Error { .. }
    ));
    assert!(matches!(run(refresh()).await, Reply::Error { .. }));
    assert_eq!(format!("{pairing:?}"), "[REDACTED]");
    // The request journal and every other profile file stay free of it.
    let mut pending = vec![dir.path().to_owned()];
    while let Some(path) = pending.pop() {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                let bytes = std::fs::read(entry.path()).unwrap();
                let token = pairing.expose().as_bytes();
                assert!(
                    !bytes.windows(token.len()).any(|w| w == token),
                    "{} holds the pairing",
                    entry.path().display()
                );
            }
        }
    }
    task.abort();
}

/// A request the scripted API received, and the feed for its response body.
struct Opened {
    target: String,
    headers: HeaderMap,
    body: Value,
    feed: tokio::sync::mpsc::Sender<Vec<u8>>,
}
#[derive(Clone)]
struct Scripted {
    opened: tokio::sync::mpsc::Sender<Opened>,
    catalog: Value,
}
/// An API whose streamed response bodies the test writes piece by piece;
/// `GET` serves `catalog`.
async fn scripted(catalog: Value) -> (String, tokio::sync::mpsc::Receiver<Opened>) {
    async fn handle(
        State(api): State<Scripted>,
        method: axum::http::Method,
        uri: axum::http::Uri,
        headers: HeaderMap,
        body: axum::body::Body,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        if method == axum::http::Method::GET {
            return Json(api.catalog).into_response();
        }
        // Requests carrying images exceed axum's default body limit.
        let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let (feed, pieces) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let opened = Opened {
            target: uri.to_string(),
            headers,
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
    let (opened, requests) = tokio::sync::mpsc::channel(8);
    let router = Router::new()
        .fallback(handle)
        .with_state(Scripted { opened, catalog });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    // Detached: the server lives as long as the test's runtime.
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{address}"), requests)
}
/// Protocol-specific stream events carrying generated data.
fn text_event(api: ProviderApi, text: &str) -> String {
    match api {
        ProviderApi::OpenAi => event(
            None,
            json!({"choices": [{"index": 0, "delta": {"content": text}}]}),
        ),
        ProviderApi::Anthropic => event(
            Some("content_block_delta"),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
        ),
        ProviderApi::Gemini => event(
            None,
            json!({"candidates": [{"content": {"parts": [{"text": text}]}}]}),
        ),
    }
}
fn opening_events(api: ProviderApi, input: u64) -> String {
    match api {
        ProviderApi::OpenAi => event(
            None,
            json!({"choices": [{"index": 0, "delta": {"role": "assistant"}}]}),
        ),
        ProviderApi::Anthropic => event(
            Some("message_start"),
            json!({"type": "message_start", "message": {"usage": {"input_tokens": input, "output_tokens": 1}}}),
        ),
        ProviderApi::Gemini => event(
            None,
            json!({"candidates": [{"content": {"parts": [{"thought": true, "text": Id::new_v4().to_string()}]}}]}),
        ),
    }
}
fn closing_events(api: ProviderApi, input: u64, output: u64) -> String {
    match api {
        ProviderApi::OpenAi => [
            event(
                None,
                json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
            ),
            event(
                None,
                json!({"choices": [], "usage": {"prompt_tokens": input, "completion_tokens": output}}),
            ),
            "data: [DONE]\n\n".to_owned(),
        ]
        .concat(),
        ProviderApi::Anthropic => [
            event(
                Some("message_delta"),
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": output}}),
            ),
            event(Some("message_stop"), json!({"type": "message_stop"})),
        ]
        .concat(),
        ProviderApi::Gemini => event(
            None,
            json!({"candidates": [{"content": {"parts": []}, "finishReason": "STOP"}], "usageMetadata": {"promptTokenCount": input, "candidatesTokenCount": output}}),
        ),
    }
}
fn error_event(api: ProviderApi, code: &str, message: &str) -> String {
    match api {
        ProviderApi::OpenAi => event(None, json!({"error": {"code": code, "message": message}})),
        ProviderApi::Anthropic => event(
            Some("error"),
            json!({"type": "error", "error": {"type": code, "message": message}}),
        ),
        ProviderApi::Gemini => event(None, json!({"error": {"status": code, "message": message}})),
    }
}
/// Send `text` split in the middle of a line, as a network might deliver it.
async fn send_split(feed: &tokio::sync::mpsc::Sender<Vec<u8>>, text: &str) {
    let (head, tail) = text.as_bytes().split_at(text.len() / 2);
    feed.send(head.to_vec()).await.unwrap();
    feed.send(tail.to_vec()).await.unwrap();
}
const PROTOCOLS: [ProviderApi; 3] = [
    ProviderApi::OpenAi,
    ProviderApi::Anthropic,
    ProviderApi::Gemini,
];
fn model_info(api: ProviderApi, provider: Id) -> ModelInfo {
    let id = Id::new_v4();
    ModelInfo {
        key: ModelKey {
            provider,
            model: if api == ProviderApi::Gemini {
                format!("models/{id}")
            } else {
                id.to_string()
            },
        },
        name: id.to_string(),
        context_limit: None,
        output_limit: None,
        input_cost_per_token: None,
        output_cost_per_token: None,
        reasoning: None,
        reasoning_efforts: vec![],
        image_input: None,
    }
}
fn conversation(info: &ModelInfo) -> Thread {
    let mut thread = Thread::new(Id::new_v4().to_string(), Mode::Chat);
    thread.model = Some(info.key.clone());
    thread.messages.push(Message {
        id: Id::new_v4(),
        role: Role::User,
        text: Id::new_v4().to_string(),
        attachments: vec![],
        images: vec![],
        at: provider::now(),
        backend: CodingBackend::Native,
        tool_calls: vec![],
        compaction: None,
        reasoning: String::new(),
        reasoning_shortened: false,
    });
    thread
}
/// The next reply text delta; reasoning (Gemini's opening thought) is
/// skipped.
async fn next_delta(deltas: &mut tokio::sync::mpsc::Receiver<Progress>) -> String {
    loop {
        let update = tokio::time::timeout(std::time::Duration::from_secs(10), deltas.recv())
            .await
            .expect("a delta must be published before the stream ends")
            .expect("the stream closed early");
        match update {
            Progress::Delta(delta) => return delta,
            Progress::Reasoning { .. } => continue,
            Progress::Text(_) | Progress::Activity(_) => panic!("native streams send deltas"),
        }
    }
}
/// Every text delta and every non-empty reasoning delta published so far,
/// and how many reasoning updates said the reasoning was shortened.
fn drain(deltas: &mut tokio::sync::mpsc::Receiver<Progress>) -> (String, Vec<String>, usize) {
    let (mut text, mut reasoning, mut shortened) = (String::new(), vec![], 0);
    while let Ok(update) = deltas.try_recv() {
        match update {
            Progress::Delta(delta) => text.push_str(&delta),
            Progress::Reasoning {
                text: delta,
                shortened: cut,
            } => {
                shortened += usize::from(cut);
                if !delta.is_empty() {
                    reasoning.push(delta);
                }
            }
            Progress::Text(_) | Progress::Activity(_) => panic!("native streams send deltas"),
        }
    }
    (text, reasoning, shortened)
}
async fn upstream_closed(feed: &tokio::sync::mpsc::Sender<Vec<u8>>, why: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(10), feed.closed())
        .await
        .unwrap_or_else(|_| panic!("{why} must abandon the provider request"));
}
/// The provider-side streaming context for one protocol: a scripted API, and
/// either a direct endpoint or a credential the broker attaches.
struct Rig {
    config: ProviderConfig,
    requests: tokio::sync::mpsc::Receiver<Opened>,
    key: Option<String>,
    broker: Option<(
        tempfile::TempDir,
        a206_core::BrokerPairing,
        provider::PairingAnchor,
    )>,
}
impl Rig {
    async fn new(api: ProviderApi, brokered: bool) -> Self {
        let (origin, requests) = scripted(json!({})).await;
        let mut config = ProviderConfig {
            id: Id::new_v4(),
            label: Id::new_v4().to_string(),
            base_url: format!("{origin}/v1"),
            credential: None,
            api,
        };
        let (mut key, mut broker) = (None, None);
        if brokered {
            let secret = Id::new_v4().to_string();
            let (dir, credential, pairing) = start_broker(&origin, &secret).await;
            let anchor = provider::verify_pairing(dir.path(), &pairing)
                .await
                .unwrap();
            config.credential = Some(credential);
            key = Some(secret);
            broker = Some((dir, pairing, anchor));
        }
        Self {
            config,
            requests,
            key,
            broker,
        }
    }
}
type Completion = tokio::task::JoinHandle<Result<(String, Usage), provider::ProviderError>>;
type TurnTask = tokio::task::JoinHandle<Result<provider::Turn, provider::ProviderError>>;
/// Run `complete` in the background, as the gateway does.
fn spawn_complete(
    rig: &Rig,
    info: &ModelInfo,
) -> (Completion, tokio::sync::mpsc::Receiver<Progress>) {
    spawn_with(
        rig,
        info,
        conversation(info),
        provider::ImageData::default(),
        |turn| (turn.text, turn.usage),
    )
}
/// Run `complete` for `thread` in the background and return the whole turn.
fn spawn_turn(
    rig: &Rig,
    info: &ModelInfo,
    thread: Thread,
) -> (TurnTask, tokio::sync::mpsc::Receiver<Progress>) {
    spawn_with(rig, info, thread, provider::ImageData::default(), |turn| {
        turn
    })
}
/// Aborting the returned task drops the request, as the gateway does.
fn spawn_with<T: Send + 'static>(
    rig: &Rig,
    info: &ModelInfo,
    thread: Thread,
    images: provider::ImageData,
    map: fn(provider::Turn) -> T,
) -> (
    tokio::task::JoinHandle<Result<T, provider::ProviderError>>,
    tokio::sync::mpsc::Receiver<Progress>,
) {
    let (progress, deltas) = tokio::sync::mpsc::channel(64);
    let config = rig.config.clone();
    let info = info.clone();
    let (root, pairing) = match &rig.broker {
        Some((dir, pairing, _)) => (dir.path().to_owned(), Some(pairing.clone())),
        None => (std::path::PathBuf::from("/nonexistent"), None),
    };
    let task = tokio::spawn(async move {
        let broker = provider::Broker {
            root: &root,
            pairing: pairing.as_ref(),
        };
        let prompt = Id::new_v4().to_string();
        provider::complete(
            &config,
            broker,
            provider::Ask {
                thread: &thread,
                prompt: &prompt,
                info: &info,
                tools: &[],
                images: &images,
            },
            &progress,
            &RetryPolicy::NONE,
        )
        .await
        .map(map)
    });
    (task, deltas)
}
#[tokio::test]
async fn deltas_are_published_in_order_while_the_stream_is_open() {
    for brokered in [false, true] {
        for api in PROTOCOLS {
            let mut rig = Rig::new(api, brokered).await;
            let info = model_info(api, rig.config.id);
            let (task, mut deltas) = spawn_complete(&rig, &info);
            let opened = rig.requests.recv().await.unwrap();
            match api {
                ProviderApi::OpenAi => {
                    assert!(opened.target.ends_with("/chat/completions"));
                    assert_eq!(opened.body["stream"], true);
                    assert_eq!(opened.body["stream_options"]["include_usage"], true);
                }
                ProviderApi::Anthropic => {
                    assert!(opened.target.ends_with("/messages"));
                    assert_eq!(opened.body["stream"], true);
                }
                ProviderApi::Gemini => {
                    let resource = info.key.model.strip_prefix("models/").unwrap();
                    assert!(
                        opened.target.ends_with(&format!(
                            "/models/{resource}:streamGenerateContent?alt=sse"
                        )),
                        "{}",
                        opened.target
                    );
                }
            }
            if let Some(key) = &rig.key {
                let (header, value) = match api {
                    ProviderApi::OpenAi => ("authorization", format!("Bearer {key}")),
                    ProviderApi::Anthropic => ("x-api-key", key.clone()),
                    ProviderApi::Gemini => ("x-goog-api-key", key.clone()),
                };
                assert_eq!(opened.headers.get(header).unwrap(), value.as_str());
            }
            let identity = Id::new_v4().as_u128();
            let (input, output) = (
                (identity % 100_000) as u64 + 1,
                (identity >> 64) as u64 % 100_000 + 1,
            );
            send_split(&opened.feed, &opening_events(api, input)).await;
            let mut expected = String::new();
            // Each delta must be published before the next one exists.
            for _ in 0..4 {
                let delta = format!("{} \u{00e9}\u{1f600}", Id::new_v4());
                send_split(&opened.feed, &text_event(api, &delta)).await;
                assert_eq!(
                    next_delta(&mut deltas).await,
                    delta,
                    "{api:?} brokered={brokered}"
                );
                expected.push_str(&delta);
            }
            send_split(&opened.feed, &closing_events(api, input, output)).await;
            drop(opened.feed);
            let (text, usage) = task.await.unwrap().unwrap();
            assert_eq!(text, expected);
            assert_eq!(usage.output_tokens, Some(output));
            assert_eq!(usage.input_tokens, Some(input));
            assert!(usage.cost.is_none());
            assert!(deltas.try_recv().is_err(), "no delta beyond the reply");
        }
    }
}
/// Protocol-specific stream events carrying reasoning text.
fn reasoning_event(api: ProviderApi, text: &str, variant: usize) -> String {
    match api {
        // Servers name the field `reasoning_content` or `reasoning`.
        ProviderApi::OpenAi => {
            let field = ["reasoning_content", "reasoning"][variant % 2];
            event(
                None,
                json!({"choices": [{"index": 0, "delta": {field: text}}]}),
            )
        }
        ProviderApi::Anthropic => event(
            Some("content_block_delta"),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": text}}),
        ),
        ProviderApi::Gemini => event(
            None,
            json!({"candidates": [{"content": {"parts": [{"thought": true, "text": text}]}}]}),
        ),
    }
}
#[tokio::test]
async fn reasoning_streams_apart_from_the_reply_and_is_never_sent_back() {
    for api in PROTOCOLS {
        let mut rig = Rig::new(api, false).await;
        let mut info = model_info(api, rig.config.id);
        info.reasoning = Some(true);
        // A conversation whose earlier reply kept its reasoning.
        let mut thread = conversation(&info);
        let earlier = Id::new_v4().to_string();
        let mut reply = Message::new(
            Role::Assistant,
            Id::new_v4().to_string(),
            provider::now(),
            CodingBackend::Native,
        );
        reply.reasoning = earlier.clone();
        thread.messages.push(reply);
        thread.messages.push(Message::new(
            Role::User,
            Id::new_v4().to_string(),
            provider::now(),
            CodingBackend::Native,
        ));
        let (task, mut deltas) = spawn_turn(&rig, &info, thread);
        let opened = rig.requests.recv().await.unwrap();
        assert!(
            !opened.body.to_string().contains(&earlier),
            "{api:?}: reasoning was sent back"
        );
        if api == ProviderApi::Gemini {
            assert_eq!(
                opened.body["generationConfig"]["thinkingConfig"]["includeThoughts"],
                true
            );
        }
        // Gemini's opening events are a thought of their own.
        if api != ProviderApi::Gemini {
            send_split(&opened.feed, &opening_events(api, 1)).await;
        }
        // Reasoning, then text, then more reasoning between the two.
        let thoughts: Vec<String> = (0..3)
            .map(|_| format!("{} \u{00e9}\u{1f600}", Id::new_v4()))
            .collect();
        let text = Id::new_v4().to_string();
        for (variant, thought) in thoughts[..2].iter().enumerate() {
            send_split(&opened.feed, &reasoning_event(api, thought, variant)).await;
        }
        send_split(&opened.feed, &text_event(api, &text)).await;
        send_split(&opened.feed, &reasoning_event(api, &thoughts[2], 0)).await;
        if api == ProviderApi::OpenAi {
            // A server that sends both names sends the text once.
            let both = json!({"choices": [{"index": 0, "delta": {"reasoning_content": text, "reasoning": text}}]});
            send_split(&opened.feed, &event(None, both)).await;
        }
        send_split(&opened.feed, &closing_events(api, 1, 1)).await;
        drop(opened.feed);
        let turn = task.await.unwrap().unwrap();
        let mut expected = thoughts.concat();
        let mut streamed_thoughts = thoughts.clone();
        if api == ProviderApi::OpenAi {
            expected.push_str(&text);
            streamed_thoughts.push(text.clone());
        }
        assert_eq!(turn.text, text, "{api:?}");
        assert_eq!(turn.reasoning, expected, "{api:?}");
        assert!(!turn.reasoning_shortened);
        let (streamed, reasoning, _) = drain(&mut deltas);
        assert_eq!(streamed, text, "{api:?}");
        assert_eq!(reasoning, streamed_thoughts, "{api:?}");

        // A model the API does not report as thinking is not asked for
        // thoughts.
        if api == ProviderApi::Gemini {
            info.reasoning = None;
            let (task, _deltas) = spawn_complete(&rig, &info);
            let opened = rig.requests.recv().await.unwrap();
            assert!(opened.body.get("generationConfig").is_none());
            drop(opened.feed);
            task.await.unwrap().unwrap_err();
        }
    }
}
#[tokio::test]
async fn reasoning_beyond_the_limit_is_cut_and_the_reply_still_completes() {
    for api in PROTOCOLS {
        let mut rig = Rig::new(api, false).await;
        let info = model_info(api, rig.config.id);
        let (task, mut deltas) = spawn_turn(&rig, &info, conversation(&info));
        let opened = rig.requests.recv().await.unwrap();
        if api != ProviderApi::Gemini {
            send_split(&opened.feed, &opening_events(api, 1)).await;
        }
        // Four-byte characters, so the limit falls inside one.
        let piece = "\u{1f600}".repeat(4097);
        let (mut sent, mut shortened) = (0, 0);
        // One more piece after the cut is dropped without another notice.
        while sent <= provider::MAX_REASONING_TEXT + piece.len() {
            send_split(&opened.feed, &reasoning_event(api, &piece, 0)).await;
            sent += piece.len();
            shortened += drain(&mut deltas).2;
        }
        let text = Id::new_v4().to_string();
        send_split(&opened.feed, &text_event(api, &text)).await;
        send_split(&opened.feed, &closing_events(api, 1, 1)).await;
        drop(opened.feed);
        let turn = task.await.unwrap().unwrap();
        shortened += drain(&mut deltas).2;
        // The stream itself says so, so a reply that later fails keeps it.
        assert_eq!(shortened, 1, "{api:?}");
        assert_eq!(turn.text, text, "{api:?}");
        assert!(turn.reasoning_shortened, "{api:?}");
        assert!(turn.reasoning.len() <= provider::MAX_REASONING_TEXT);
        assert!(turn.reasoning.len() > provider::MAX_REASONING_TEXT - 4);
        assert!(turn.reasoning.chars().all(|c| c == '\u{1f600}'));
    }
}
#[tokio::test]
async fn tool_call_markup_written_as_text_is_removed_from_the_reply() {
    for api in PROTOCOLS {
        let mut rig = Rig::new(api, false).await;
        let info = model_info(api, rig.config.id);
        let (intro, outro) = (Id::new_v4().to_string(), Id::new_v4().to_string());
        let call = format!("{{\"name\": \"{}\"}}", Id::new_v4());
        // The open tag is split across deltas, and the reply ends in text that
        // could have begun one.
        let chunks = [
            format!("{intro}<tool_"),
            format!("call>{call}</tool_call> `<tool_call>` {outro} <"),
        ];
        let (task, mut deltas) = spawn_complete(&rig, &info);
        let opened = rig.requests.recv().await.unwrap();
        let identity = Id::new_v4().as_u128();
        let (input, output) = (
            (identity % 1000) as u64 + 1,
            (identity >> 64) as u64 % 1000 + 1,
        );
        send_split(&opened.feed, &opening_events(api, input)).await;
        for chunk in &chunks {
            send_split(&opened.feed, &text_event(api, chunk)).await;
        }
        send_split(&opened.feed, &closing_events(api, input, output)).await;
        drop(opened.feed);
        let (text, _) = task.await.unwrap().unwrap();
        assert_eq!(text, format!("{intro} `<tool_call>` {outro} <"), "{api:?}");
        let (streamed, ..) = drain(&mut deltas);
        assert!(!streamed.contains(&call), "{api:?}: {streamed}");
        // The held `<` is the last delta.
        assert_eq!(streamed, text, "{api:?}");

        // A reply that was nothing but a leaked call is a failed reply.
        let (task, _deltas) = spawn_complete(&rig, &info);
        let opened = rig.requests.recv().await.unwrap();
        send_split(&opened.feed, &opening_events(api, input)).await;
        send_split(
            &opened.feed,
            &text_event(api, &format!("<tool_call>{call}</tool_call>\n")),
        )
        .await;
        send_split(&opened.feed, &closing_events(api, input, output)).await;
        drop(opened.feed);
        let error = task.await.unwrap().unwrap_err();
        assert!(
            matches!(error, provider::ProviderError::Malformed(why) if why.contains("tool call as text")),
            "{api:?}: {error:?}"
        );
    }
}
#[tokio::test]
async fn malformed_failed_and_truncated_streams_are_typed_errors() {
    use provider::ProviderError as P;
    for brokered in [false, true] {
        for api in PROTOCOLS {
            let mut rig = Rig::new(api, brokered).await;
            let info = model_info(api, rig.config.id);
            let code = Id::new_v4().simple().to_string();
            let secret = Id::new_v4().to_string();
            let cases: [(Vec<u8>, &str); 5] = [
                (b"data: {not json\n\n".to_vec(), "json"),
                (b"data: \xff\xfe\n\n".to_vec(), "utf8"),
                (
                    format!("data: {}\n", "x".repeat(a206_runtime::sse::MAX_EVENT)).into_bytes(),
                    "oversized",
                ),
                (error_event(api, &code, &secret).into_bytes(), "upstream"),
                (vec![], "truncated"),
            ];
            for (bytes, case) in cases {
                let (task, mut deltas) = spawn_complete(&rig, &info);
                let opened = rig.requests.recv().await.unwrap();
                // Text before the failure is published, yet the call fails.
                let delta = Id::new_v4().to_string();
                send_split(&opened.feed, &text_event(api, &delta)).await;
                assert_eq!(next_delta(&mut deltas).await, delta);
                if !bytes.is_empty() {
                    opened.feed.send(bytes).await.unwrap();
                }
                drop(opened.feed);
                let error = task.await.unwrap().unwrap_err();
                let matched = match case {
                    "json" => matches!(error, P::Event(_)),
                    "utf8" => matches!(error, P::Sse(a206_runtime::sse::SseError::Utf8)),
                    "oversized" => matches!(error, P::Sse(a206_runtime::sse::SseError::TooLarge)),
                    "upstream" => matches!(&error, P::Upstream { code: Some(c) } if *c == code),
                    _ => matches!(error, P::Incomplete),
                };
                assert!(matched, "{api:?} brokered={brokered} {case}: {error:?}");
                // Upstream messages can echo prompts or keys; only the code is kept.
                assert!(!format!("{error} {error:?}").contains(&secret));
                let report = a206_runtime::error::report(&error);
                assert_eq!(report.kind, ErrorKind::Provider, "{case}");
            }
        }
    }
}
#[tokio::test]
async fn openai_needs_a_finish_reason_and_done_and_text_survives_a_bad_read() {
    use provider::ProviderError as P;
    let api = ProviderApi::OpenAi;
    let mut rig = Rig::new(api, false).await;
    let info = model_info(api, rig.config.id);
    let finish = event(
        None,
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
    );
    // A connection dropped after the finish chunk, and a bare `[DONE]`, are
    // both unfinished replies.
    for tail in [finish, "data: [DONE]\n\n".to_owned()] {
        let (task, mut deltas) = spawn_complete(&rig, &info);
        let opened = rig.requests.recv().await.unwrap();
        let delta = Id::new_v4().to_string();
        send_split(&opened.feed, &text_event(api, &delta)).await;
        assert_eq!(next_delta(&mut deltas).await, delta);
        opened.feed.send(tail.into_bytes()).await.unwrap();
        drop(opened.feed);
        let error = task.await.unwrap().unwrap_err();
        assert!(matches!(error, P::Incomplete), "{error:?}");
    }
    // Text completed in the same read as a decoding error is still published.
    let (task, mut deltas) = spawn_complete(&rig, &info);
    let opened = rig.requests.recv().await.unwrap();
    let delta = Id::new_v4().to_string();
    let read = [
        text_event(api, &delta).into_bytes(),
        b"data: \xff\n".to_vec(),
    ]
    .concat();
    opened.feed.send(read).await.unwrap();
    drop(opened.feed);
    let error = task.await.unwrap().unwrap_err();
    assert!(
        matches!(error, P::Sse(a206_runtime::sse::SseError::Utf8)),
        "{error:?}"
    );
    assert_eq!(next_delta(&mut deltas).await, delta);
}
#[tokio::test]
async fn cancellation_and_vault_lock_stop_a_streaming_request() {
    for api in PROTOCOLS {
        let mut rig = Rig::new(api, true).await;
        let info = model_info(api, rig.config.id);
        // Dropping the run (as the gateway does on cancel) closes the upstream.
        let (task, mut deltas) = spawn_complete(&rig, &info);
        let opened = rig.requests.recv().await.unwrap();
        let delta = Id::new_v4().to_string();
        send_split(&opened.feed, &text_event(api, &delta)).await;
        assert_eq!(next_delta(&mut deltas).await, delta);
        task.abort();
        upstream_closed(&opened.feed, "cancellation").await;

        // Locking the vault ends the stream with a structured broker error.
        let (task, mut deltas) = spawn_complete(&rig, &info);
        let opened = rig.requests.recv().await.unwrap();
        send_split(&opened.feed, &text_event(api, &delta)).await;
        assert_eq!(next_delta(&mut deltas).await, delta);
        let root = rig.broker.as_ref().unwrap().0.path().to_owned();
        let mut user = a206_broker::Client::connect(&root, a206_broker::Role::User)
            .await
            .unwrap();
        user.call(a206_broker::Request::Lock).await.unwrap();
        let error = task.await.unwrap().unwrap_err();
        assert!(
            matches!(
                &error,
                provider::ProviderError::Broker(a206_broker::ClientError::Broker(
                    a206_broker::BrokerError::Locked
                ))
            ),
            "{error:?}"
        );
        assert_eq!(
            a206_runtime::error::report(&error).kind,
            ErrorKind::Unavailable
        );
        upstream_closed(&opened.feed, "a vault lock").await;
    }
}
/// Streamed text reaches clients as durable, cursor-ordered thread events
/// before the reply completes; failures and cancellation keep partial text
/// but never mark the run complete.
#[tokio::test]
async fn the_gateway_journals_streamed_text_and_reports_partial_failures() {
    let model = Id::new_v4().to_string();
    let (origin, mut requests) = scripted(json!({"data": [{"id": model}]})).await;
    let dir = tempfile::tempdir().unwrap();
    let gateway = a206_runtime::gateway::Gateway::open(dir.path()).unwrap();
    let run = |body| {
        let gateway = gateway.clone();
        async move {
            gateway
                .request(a206_protocol::Request {
                    id: Id::new_v4(),
                    body,
                })
                .await
                .body
        }
    };
    let provider = Id::new_v4();
    let mut settings = Settings::default();
    settings.providers.push(ProviderConfig {
        id: provider,
        label: Id::new_v4().to_string(),
        base_url: format!("{origin}/v1"),
        credential: None,
        api: ProviderApi::OpenAi,
    });
    assert_eq!(
        run(Command::SaveSettings(Box::new(settings))).await,
        Reply::Ok
    );
    let Reply::Catalog(catalog) = run(Command::RefreshCatalog { provider }).await else {
        panic!("catalog refresh failed");
    };
    let Reply::Thread(thread) = run(Command::CreateThread {
        title: Id::new_v4().to_string(),
        mode: Mode::Chat,
    })
    .await
    else {
        panic!("thread creation failed");
    };
    let id = thread.id;
    assert!(matches!(
        run(Command::ConfigureThread {
            id,
            mode: Mode::Chat,
            model: Some(catalog.models[0].key.clone()),
            subagent_models: Default::default(),
        })
        .await,
        Reply::Thread(_)
    ));
    let assistant_text = |gateway: &a206_runtime::gateway::Gateway| {
        let state = gateway.snapshot().unwrap();
        let thread = state.threads.iter().find(|t| t.id == id).unwrap().clone();
        thread
            .messages
            .iter()
            .rev()
            .take_while(|m| m.role != Role::User)
            .find(|m| m.role == Role::Assistant)
            .map(|m| m.text.clone())
    };
    let wait_for = |expected: String| {
        let gateway = gateway.clone();
        async move {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while assistant_text(&gateway).as_deref() != Some(&expected) {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("streamed text was not published"));
        }
    };
    let send = || {
        let gateway = gateway.clone();
        tokio::spawn(async move {
            gateway
                .request(a206_protocol::Request {
                    id: Id::new_v4(),
                    body: Command::Send {
                        thread: id,
                        text: Id::new_v4().to_string(),
                        attachments: vec![],
                        images: vec![],
                    },
                })
                .await
                .body
        })
    };
    let api = ProviderApi::OpenAi;

    // Success: each delta is visible in the journal before the next is sent,
    // and the final reply replaces (not duplicates) the partial message.
    let Reply::Events(before) = run(Command::Events { after: 0 }).await else {
        panic!("events unavailable");
    };
    let cursor = before.last().map_or(0, |e| e.sequence);
    let reply = send();
    let opened = requests.recv().await.unwrap();
    // Reasoning is published on the reply before any text exists.
    let thought = Id::new_v4().to_string();
    send_split(&opened.feed, &reasoning_event(api, &thought, 0)).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let state = gateway.snapshot().unwrap();
            let thread = state.threads.iter().find(|t| t.id == id).unwrap();
            if thread
                .messages
                .last()
                .is_some_and(|m| m.reasoning == thought)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("streamed reasoning was not published");
    let mut expected = String::new();
    for _ in 0..3 {
        let delta = Id::new_v4().to_string();
        send_split(&opened.feed, &text_event(api, &delta)).await;
        expected.push_str(&delta);
        wait_for(expected.clone()).await;
    }
    let Reply::Events(events) = run(Command::Events { after: cursor }).await else {
        panic!("events unavailable");
    };
    assert!(events.windows(2).all(|w| w[0].sequence < w[1].sequence));
    let changes = events
        .iter()
        .filter(|e| e.kind == EventKind::ThreadChanged { id })
        .count();
    // The user message, then at least one publication per observed delta.
    assert!(changes >= 4, "{changes} thread events for 3 deltas");
    send_split(&opened.feed, &closing_events(api, 1, 1)).await;
    drop(opened.feed);
    let Reply::Thread(done) = reply.await.unwrap() else {
        panic!("send failed");
    };
    assert_eq!(done.state, RunState::Complete);
    assert_eq!(done.messages.len(), 2);
    assert_eq!(done.messages[1].text, expected);
    assert_eq!(done.messages[1].reasoning, thought);
    assert!(!done.messages[1].reasoning_shortened);

    // Failure mid-stream: the partial text stays, the run is Failed, and the
    // structured report reaches the client that started it.
    let reply = send();
    let opened = requests.recv().await.unwrap();
    let mut partial = Id::new_v4().to_string();
    send_split(&opened.feed, &text_event(api, &partial)).await;
    wait_for(partial.clone()).await;
    // Text that arrives just before the failure, possibly before it was
    // published, still belongs to the failed reply.
    let late = Id::new_v4().to_string();
    let code = Id::new_v4().simple().to_string();
    let tail = [
        text_event(api, &late),
        error_event(api, &code, &Id::new_v4().to_string()),
    ]
    .concat();
    partial.push_str(&late);
    send_split(&opened.feed, &tail).await;
    drop(opened.feed);
    let Reply::Thread(failed) = reply.await.unwrap() else {
        panic!("send failed");
    };
    assert_eq!(failed.state, RunState::Failed);
    let report = failed.error.clone().unwrap();
    assert_eq!(report.kind, ErrorKind::Provider);
    assert!(report.message.contains(&code), "{}", report.message);
    assert!(report.correlation.is_some());
    let tail = &failed.messages[failed.messages.len() - 2..];
    assert_eq!(tail[0].role, Role::Assistant);
    assert_eq!(tail[0].text, partial);
    assert_eq!(tail[1].role, Role::System);
    assert_eq!(
        failed.usage, done.usage,
        "a failed run does not replace the last usage"
    );

    // Cancellation: the upstream request is abandoned and the run is Cancelled.
    let reply = send();
    let opened = requests.recv().await.unwrap();
    let partial = Id::new_v4().to_string();
    send_split(&opened.feed, &text_event(api, &partial)).await;
    wait_for(partial.clone()).await;
    assert_eq!(run(Command::CancelRun { thread: id }).await, Reply::Ok);
    let Reply::Thread(cancelled) = reply.await.unwrap() else {
        panic!("send failed");
    };
    assert_eq!(cancelled.state, RunState::Cancelled);
    assert_eq!(cancelled.messages.last().unwrap().text, partial);
    upstream_closed(&opened.feed, "cancellation").await;

    // Each run's end is journaled once, with how it ended.
    let Reply::Events(events) = run(Command::Events { after: cursor }).await else {
        panic!("events unavailable");
    };
    let ended: Vec<_> = events
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::RunEnded { thread, state } if thread == id => Some(state),
            _ => None,
        })
        .collect();
    assert_eq!(
        ended,
        [RunState::Complete, RunState::Failed, RunState::Cancelled]
    );
}
/// A broker from before streaming answers in one buffered reply; the gateway
/// greets it with its own version, never sends it a stream request, and
/// decodes the buffered stream body the same way.
#[cfg(unix)]
#[tokio::test]
async fn an_older_broker_is_used_without_streaming() {
    use a206_broker::{BrokerError, InvalidRequest, Reply, Request};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let api = ProviderApi::Anthropic;
    let text = Id::new_v4().to_string();
    let (input, output) = (Id::new_v4().as_u128() as u64 % 1000 + 1, 1);
    let body = [
        opening_events(api, input),
        text_event(api, &text),
        closing_events(api, input, output),
    ]
    .concat()
    .into_bytes();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("vault")).unwrap();
    let listener = tokio::net::UnixListener::bind(a206_broker::socket_path(dir.path())).unwrap();
    let older = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut decoder = a206_protocol::Decoder::default();
        let mut bytes = vec![0; 8192];
        let mut seen = vec![];
        loop {
            let count = stream.read(&mut bytes).await.unwrap();
            if count == 0 {
                return seen;
            }
            let requests: Vec<a206_protocol::Request<Request>> =
                decoder.feed(&bytes[..count]).unwrap();
            for request in requests {
                let reply = match &request.body {
                    Request::Hello { version, .. }
                        if *version == a206_broker::MIN_BROKER_VERSION =>
                    {
                        Reply::Ok
                    }
                    Request::Hello { .. } => {
                        Reply::Error(BrokerError::Invalid(InvalidRequest::UnsupportedVersion))
                    }
                    Request::GatewayAuth { .. } => Reply::Ok,
                    Request::Provider(_) => Reply::Provider {
                        status: 200,
                        body: body.clone(),
                    },
                    _ => Reply::Error(BrokerError::Forbidden),
                };
                seen.push(match request.body {
                    Request::Hello { version, .. } => format!("hello {version}"),
                    Request::GatewayAuth { .. } => "pair".into(),
                    Request::Provider(_) => "provider".into(),
                    _ => "other".into(),
                });
                let frame = a206_protocol::encode(&a206_protocol::Response {
                    id: request.id,
                    body: reply,
                })
                .unwrap();
                stream.write_all(&frame).await.unwrap();
            }
        }
    });
    let config = ProviderConfig {
        id: Id::new_v4(),
        label: Id::new_v4().to_string(),
        base_url: "https://example.invalid/v1".into(),
        credential: Some(Id::new_v4().to_string()),
        api,
    };
    let info = model_info(api, config.id);
    let pairing = a206_core::BrokerPairing::new(Id::new_v4().to_string());
    let broker = provider::Broker {
        root: dir.path(),
        pairing: Some(&pairing),
    };
    let (progress, mut deltas) = tokio::sync::mpsc::channel(8);
    let thread = conversation(&info);
    let provider::Turn {
        text: got, usage, ..
    } = provider::complete(
        &config,
        broker,
        provider::Ask {
            thread: &thread,
            prompt: "",
            info: &info,
            tools: &[],
            images: &provider::ImageData::default(),
        },
        &progress,
        &RetryPolicy::NONE,
    )
    .await
    .unwrap();
    assert_eq!(got, text);
    assert_eq!(next_delta(&mut deltas).await, text);
    assert_eq!(usage.input_tokens, Some(input));
    let seen = older.await.unwrap();
    // Each newer greeting is refused down to the oldest, which is accepted;
    // no stream request follows.
    let mut expected: Vec<String> = (a206_broker::MIN_BROKER_VERSION..=a206_broker::BROKER_VERSION)
        .rev()
        .map(|version| format!("hello {version}"))
        .collect();
    expected.extend(["pair".into(), "provider".into()]);
    assert_eq!(seen, expected);
}

/// A credentialed request whose connection the broker could not open is
/// retried like a direct one, and the broker's typed failure is returned
/// once the attempts run out.
#[tokio::test]
async fn brokered_connection_failures_are_retried() {
    // Port 0 can never be listened on, so every connection is refused.
    let origin = "http://127.0.0.1:0".to_owned();
    let (dir, credential, pairing) = start_broker(&origin, &Id::new_v4().to_string()).await;
    let _anchor = provider::verify_pairing(dir.path(), &pairing)
        .await
        .unwrap();
    let api = ProviderApi::OpenAi;
    let config = ProviderConfig {
        id: Id::new_v4(),
        label: Id::new_v4().to_string(),
        base_url: format!("{origin}/v1"),
        credential: Some(credential),
        api,
    };
    let info = model_info(api, config.id);
    let thread = conversation(&info);
    let (progress, mut notices) = tokio::sync::mpsc::channel(64);
    let retry = RetryPolicy {
        attempts: 2,
        base: std::time::Duration::ZERO,
        max: std::time::Duration::ZERO,
        max_retry_after: std::time::Duration::ZERO,
    };
    let error = provider::complete(
        &config,
        provider::Broker {
            root: dir.path(),
            pairing: Some(&pairing),
        },
        provider::Ask {
            thread: &thread,
            prompt: "",
            info: &info,
            tools: &[],
            images: &provider::ImageData::default(),
        },
        &progress,
        &retry,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            error,
            provider::ProviderError::Broker(a206_broker::ClientError::Broker(
                a206_broker::BrokerError::Upstream(a206_broker::UpstreamFailure::Connect)
            ))
        ),
        "{error:?}"
    );
    drop(progress);
    let mut retries = vec![];
    while let Some(notice) = notices.recv().await {
        if let Progress::Activity(text) = notice {
            retries.push(text);
        }
    }
    assert_eq!(retries.len(), 1, "{retries:?}");
    assert!(retries[0].contains("2 of 2"), "{retries:?}");
}
/// A PNG signature followed by `len` bytes unique to this run.
fn png(len: usize) -> Vec<u8> {
    let mut data = b"\x89PNG\r\n\x1a\n".to_vec();
    while data.len() < len {
        data.extend_from_slice(Id::new_v4().as_bytes());
    }
    data.truncate(len.max(8));
    data
}
/// The parts of the request's first user message.
fn first_user_parts(api: ProviderApi, body: &Value) -> Vec<Value> {
    let parts = match api {
        // The system prompt comes first.
        ProviderApi::OpenAi => &body["messages"][1]["content"],
        ProviderApi::Anthropic => &body["messages"][0]["content"],
        ProviderApi::Gemini => &body["contents"][0]["parts"],
    };
    parts.as_array().cloned().unwrap_or_default()
}
#[tokio::test]
async fn attached_images_are_sent_in_each_protocols_form_unless_the_api_says_no() {
    use base64::Engine;
    for api in PROTOCOLS {
        let mut rig = Rig::new(api, false).await;
        let mut info = model_info(api, rig.config.id);
        let dir = tempfile::tempdir().unwrap();
        let store = a206_runtime::images::Images::new(dir.path());
        let data = png(64);
        let image = store.stage(&Id::new_v4().to_string(), &data).unwrap();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
        let mut thread = conversation(&info);
        thread.messages[0].images.push(image.clone());
        let text = thread.messages[0].text.clone();
        // Unreported is left to the provider; reported support sends too.
        for accepts in [None, Some(true), Some(false)] {
            info.image_input = accepts;
            let images = provider::ImageData::load(&store, &thread, &info).unwrap();
            let (task, _deltas) = spawn_with(&rig, &info, thread.clone(), images, |t| t.text);
            let opened = rig.requests.recv().await.unwrap();
            let parts = first_user_parts(api, &opened.body);
            if accepts == Some(false) {
                assert!(!opened.body.to_string().contains(&encoded), "{api:?}");
                assert_eq!(parts.len(), 1, "{api:?}: {parts:?}");
                let note = parts[0]["text"].as_str().unwrap();
                assert!(
                    note.starts_with(&text) && note.contains(&image.name),
                    "{api:?}: {note}"
                );
                assert!(note.contains("does not accept images"), "{api:?}: {note}");
            } else {
                let expected_image = match api {
                    ProviderApi::OpenAi => json!({"type": "image_url",
                        "image_url": {"url": format!("data:image/png;base64,{encoded}")}}),
                    ProviderApi::Anthropic => json!({"type": "image",
                        "source": {"type": "base64", "media_type": "image/png", "data": encoded}}),
                    ProviderApi::Gemini => {
                        json!({"inlineData": {"mimeType": "image/png", "data": encoded}})
                    }
                };
                let expected_text = match api {
                    ProviderApi::Gemini => json!({"text": text}),
                    _ => json!({"type": "text", "text": text}),
                };
                assert_eq!(parts, vec![expected_image, expected_text], "{api:?}");
            }
            drop(opened.feed);
            task.await.unwrap().unwrap_err();
        }
        // An image alone sends no empty text part.
        info.image_input = None;
        thread.messages[0].text.clear();
        let images = provider::ImageData::load(&store, &thread, &info).unwrap();
        let (task, _deltas) = spawn_with(&rig, &info, thread.clone(), images, |t| t.text);
        let opened = rig.requests.recv().await.unwrap();
        assert_eq!(first_user_parts(api, &opened.body).len(), 1, "{api:?}");
        drop(opened.feed);
        task.await.unwrap().unwrap_err();
    }
}
#[tokio::test]
async fn older_images_are_named_not_resent_when_the_request_would_be_too_large() {
    let api = ProviderApi::Anthropic;
    let mut rig = Rig::new(api, false).await;
    let info = model_info(api, rig.config.id);
    let dir = tempfile::tempdir().unwrap();
    let store = a206_runtime::images::Images::new(dir.path());
    let mut thread = conversation(&info);
    // Three images of the largest size: their base64 forms cannot all fit.
    let mut staged = vec![];
    for _ in 0..3 {
        let image = store
            .stage(&Id::new_v4().to_string(), &png(a206_core::MAX_IMAGE_BYTES))
            .unwrap();
        let mut message = Message::new(
            Role::User,
            Id::new_v4().to_string(),
            provider::now(),
            CodingBackend::Native,
        );
        message.images.push(image.clone());
        thread.messages.push(Message::new(
            Role::Assistant,
            Id::new_v4().to_string(),
            provider::now(),
            CodingBackend::Native,
        ));
        thread.messages.push(message);
        staged.push(image);
    }
    let images = provider::ImageData::load(&store, &thread, &info).unwrap();
    let (mut task, _deltas) = spawn_with(&rig, &info, thread, images, |t| t.text);
    let opened = tokio::select! {
        opened = rig.requests.recv() => opened.unwrap(),
        failed = &mut task => panic!("no request was made: {failed:?}"),
    };
    let body = opened.body.to_string();
    assert!(
        body.len() <= a206_broker::MAX_PROVIDER_BODY,
        "{}",
        body.len()
    );
    let messages = opened.body["messages"].as_array().unwrap();
    let sent = |index: usize| {
        messages[index]["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|part| part["type"] == "image")
    };
    // The newest image is sent; the older ones are named in their messages.
    let newest = messages.len() - 1;
    assert!(sent(newest));
    for older in [newest - 2, newest - 4] {
        assert!(!sent(older));
        let note = messages[older]["content"][0]["text"].as_str().unwrap();
        assert!(note.contains("too large"), "{note}");
    }
    assert!(body.contains(&staged[0].name) && body.contains(&staged[1].name));
    drop(opened.feed);
    task.await.unwrap().unwrap_err();
}
#[tokio::test]
async fn image_support_is_read_from_the_models_api_and_unknown_when_unreported() {
    let reported = |api: ProviderApi, accepts: Option<bool>| -> Value {
        let id = Id::new_v4().to_string();
        match (api, accepts) {
            (ProviderApi::Anthropic, Some(accepts)) => {
                json!({"id": id, "capabilities": {"image_input": {"supported": accepts}}})
            }
            (ProviderApi::OpenAi, Some(accepts)) => {
                let modalities = if accepts {
                    json!(["text", "image"])
                } else {
                    json!(["text"])
                };
                json!({"id": id, "architecture": {"input_modalities": modalities}})
            }
            (ProviderApi::Gemini, _) => json!({"name": format!("models/{id}"),
                "supportedGenerationMethods": ["generateContent"]}),
            (_, None) => json!({"id": id}),
        }
    };
    for api in PROTOCOLS {
        let cases: &[Option<bool>] = if api == ProviderApi::Gemini {
            &[None]
        } else {
            &[Some(true), Some(false), None]
        };
        let items: Vec<Value> = cases.iter().map(|c| reported(api, *c)).collect();
        let catalog = match api {
            ProviderApi::Gemini => json!({"models": items}),
            _ => json!({"data": items}),
        };
        let (origin, _requests) = scripted(catalog).await;
        let config = ProviderConfig {
            id: Id::new_v4(),
            label: Id::new_v4().to_string(),
            base_url: format!("{origin}/v1"),
            credential: None,
            api,
        };
        let none = tempfile::tempdir().unwrap();
        let broker = provider::Broker {
            root: none.path(),
            pairing: None,
        };
        let catalog = provider::catalog(&config, broker).await.unwrap();
        let found: Vec<Option<bool>> = catalog.models.iter().map(|m| m.image_input).collect();
        assert_eq!(found, cases, "{api:?}");
    }
}
