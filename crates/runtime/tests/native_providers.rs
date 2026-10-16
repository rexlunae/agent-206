//! Native protocol contracts against a live API with generated model facts.
use a206_core::*;
use a206_runtime::{backends::Progress, provider};
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
            at: provider::now(),
            backend: CodingBackend::Native,
        });
        if protocol == ProviderApi::Anthropic {
            thread.effort = Some(api.effort.clone());
            assert_eq!(model.reasoning_efforts, vec![api.effort.clone()]);
        }
        let prompt = Id::new_v4().to_string();
        let (progress, mut deltas) = tokio::sync::mpsc::channel(64);
        let (text, usage) = provider::complete(&config, broker, &thread, &prompt, model, &progress)
            .await
            .unwrap();
        assert_eq!(text, api.text);
        let mut streamed = String::new();
        while let Ok(update) = deltas.try_recv() {
            let Progress::Delta(delta) = update else {
                panic!("native streams send deltas");
            };
            streamed.push_str(&delta);
        }
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
    assert_eq!(run(Command::SaveSettings(settings)).await, Reply::Ok);
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
        body: axum::body::Bytes,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        if method == axum::http::Method::GET {
            return Json(api.catalog).into_response();
        }
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
        at: provider::now(),
        backend: CodingBackend::Native,
    });
    thread
}
async fn next_delta(deltas: &mut tokio::sync::mpsc::Receiver<Progress>) -> String {
    let update = tokio::time::timeout(std::time::Duration::from_secs(10), deltas.recv())
        .await
        .expect("a delta must be published before the stream ends")
        .expect("the stream closed early");
    let Progress::Delta(delta) = update else {
        panic!("native streams send deltas");
    };
    delta
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
/// Run `complete` in the background, as the gateway does.
fn spawn_complete(
    rig: &Rig,
    info: &ModelInfo,
) -> (Completion, tokio::sync::mpsc::Receiver<Progress>) {
    let (progress, deltas) = tokio::sync::mpsc::channel(64);
    let config = rig.config.clone();
    let info = info.clone();
    let thread = conversation(&info);
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
        provider::complete(&config, broker, &thread, &prompt, &info, &progress).await
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
    assert_eq!(run(Command::SaveSettings(settings)).await, Reply::Ok);
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
    let (got, usage) = provider::complete(&config, broker, &thread, "", &info, &progress)
        .await
        .unwrap();
    assert_eq!(got, text);
    assert_eq!(next_delta(&mut deltas).await, text);
    assert_eq!(usage.input_tokens, Some(input));
    let seen = older.await.unwrap();
    // The newest greeting is refused, the older one accepted; no stream request.
    let (newest, oldest) = (a206_broker::BROKER_VERSION, a206_broker::MIN_BROKER_VERSION);
    assert_eq!(
        seen,
        [
            format!("hello {newest}"),
            format!("hello {oldest}"),
            "pair".into(),
            "provider".into()
        ]
    );
}
