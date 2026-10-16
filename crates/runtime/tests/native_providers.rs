//! Native protocol contracts against a live API with generated model facts.
use a206_core::*;
use a206_runtime::provider;
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
async fn messages(
    State(api): State<Api>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    assert_key(&api, &headers);
    assert_eq!(headers.get("anthropic-version").unwrap(), "2023-06-01");
    api.requests.lock().unwrap().push(body);
    Json(
        json!({"content": [{"type": "thinking", "thinking": Id::new_v4().to_string()}, {"type": "text", "text": api.text}], "usage": {"input_tokens": api.limit, "output_tokens": api.output}}),
    )
}
async fn generate(
    State(api): State<Api>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    assert_key(&api, &headers);
    api.requests.lock().unwrap().push(body);
    Json(
        json!({"candidates": [{"content": {"parts": [{"thought": true, "text": Id::new_v4().to_string()}, {"text": api.text}]}}], "usageMetadata": {"promptTokenCount": api.limit, "candidatesTokenCount": api.output}}),
    )
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
                &format!("/v1/models/{}:generateContent", api.id),
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
        assert!(
            provider::catalog(&config, unpaired)
                .await
                .unwrap_err()
                .to_string()
                .contains("not paired")
        );
        let forged = a206_core::BrokerPairing::new(Id::new_v4().to_string());
        let forged = provider::Broker {
            root: broker.path(),
            pairing: Some(&forged),
        };
        assert!(provider::catalog(&config, forged).await.is_err());
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
        let (text, usage) = provider::complete(&config, broker, &thread, &prompt, model)
            .await
            .unwrap();
        assert_eq!(text, api.text);
        assert_eq!(usage.input_tokens, Some(api.limit));
        assert_eq!(usage.output_tokens, Some(api.output));
        let request = api.requests.lock().unwrap()[0].clone();
        match protocol {
            ProviderApi::Anthropic => {
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
        Reply::Error { message } if message.contains("not paired")
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
    let Reply::Catalog(catalog) = run(refresh()).await else {
        panic!("paired refresh failed");
    };
    assert_eq!(catalog.models[0].key.model, api.id);
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
