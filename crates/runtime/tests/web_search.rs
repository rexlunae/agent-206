//! A model searches the web with `web_search`: the vault broker attaches
//! each service's key, the model reads only the results, and the tool needs
//! the conversation's network grant. Every identifier, query, key and result
//! is generated at runtime.
mod support;

use a206_core::{web_search::*, *};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::*;

type Gateway = a206_runtime::gateway::Gateway;

/// Stand-ins for Brave Search and Exa that answer only their own key, with
/// one result naming the query, and record what each was asked.
#[derive(Clone)]
struct Services {
    brave_key: String,
    exa_key: String,
    asked: Arc<Mutex<Vec<String>>>,
}
fn result_url(query: &str) -> String {
    format!("https://{query}.example/page")
}
async fn serve(services: Services) -> String {
    let router = Router::new()
        .route(
            "/res/v1/web/search",
            get(
                |State(services): State<Services>,
                 headers: HeaderMap,
                 Query(query): Query<HashMap<String, String>>| async move {
                    if headers.get("x-subscription-token").and_then(|v| v.to_str().ok())
                        != Some(services.brave_key.as_str())
                    {
                        return Err(StatusCode::UNAUTHORIZED);
                    }
                    let q = query["q"].clone();
                    services.asked.lock().unwrap().push(q.clone());
                    Ok(Json(json!({"web": {"results": [
                        {"title": format!("<strong>{q}</strong>"), "url": result_url(&q), "description": q}
                    ]}})))
                },
            ),
        )
        .route(
            "/search",
            post(
                |State(services): State<Services>, headers: HeaderMap, Json(body): Json<Value>| async move {
                    if headers.get("x-api-key").and_then(|v| v.to_str().ok())
                        != Some(services.exa_key.as_str())
                    {
                        return Err(StatusCode::FORBIDDEN);
                    }
                    let q = body["query"].as_str().unwrap().to_owned();
                    services.asked.lock().unwrap().push(q.clone());
                    Ok(Json(json!({"results": [{"title": q, "url": result_url(&q), "text": q}]})))
                },
            ),
        )
        .with_state(services);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    // Detached: the server lives as long as the test's runtime.
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    origin
}

/// An unlocked broker in a new profile holding `keys` as credentials bound
/// to `origin`; their names and a gateway pairing.
async fn broker(origin: &str, keys: &[&str]) -> (tempfile::TempDir, Vec<String>, BrokerPairing) {
    use a206_broker::*;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_owned();
    let serving = root.clone();
    // Detached: the broker lives as long as the test's runtime.
    tokio::spawn(async move { serve(&serving).await.unwrap() });
    let mut user = loop {
        match Client::connect(&root, Role::User).await {
            Ok(client) => break client,
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    };
    user.call(Request::Unlock {
        password: WireSecret::new(nonce()),
        totp: None,
        create: true,
    })
    .await
    .unwrap();
    let mut names = vec![];
    for key in keys {
        let name = nonce();
        user.call(Request::PutCredential {
            name: name.clone(),
            meta: a206_vault::CredentialMeta {
                label: name.clone(),
                kind: a206_vault::CredentialKind::ApiKey,
                policy: a206_vault::AccessPolicy::Always,
                description: None,
                disabled: false,
                destinations: vec![origin.to_owned()],
            },
            value: WireValue::Single(WireSecret::new((*key).to_owned())),
            replace: false,
        })
        .await
        .unwrap();
        names.push(name);
    }
    let Reply::GatewayPairing { token } = user.call(Request::PairGateway).await.unwrap() else {
        panic!("expected a pairing");
    };
    (dir, names, BrokerPairing::new(token.expose().to_owned()))
}

async fn ok(gateway: &Gateway, command: Command) {
    let reply = request(gateway, command).await;
    assert!(matches!(reply, Reply::Ok | Reply::Thread(_)), "{reply:?}");
}

#[tokio::test]
async fn a_model_searches_through_the_broker_and_never_sees_the_keys() {
    let api = ProviderApi::OpenAi;
    let services = Services {
        brave_key: nonce(),
        exa_key: nonce(),
        asked: Arc::default(),
    };
    let origin = serve(services.clone()).await;
    let wrong = nonce();
    let (state, names, pairing) =
        broker(&origin, &[&services.brave_key, &services.exa_key, &wrong]).await;
    let gateway = Gateway::open(state.path()).unwrap();
    let (model, mut requests) = connect(&gateway, api).await;
    let tool = a206_runtime::search_tool::SEARCH;

    let Reply::Thread(thread) = request(
        &gateway,
        Command::CreateThread {
            title: nonce(),
            mode: Mode::Research,
        },
    )
    .await
    else {
        panic!("thread created");
    };
    let thread = thread.id;
    ok(
        &gateway,
        Command::ConfigureThread {
            id: thread,
            mode: Mode::Research,
            model: Some(model),
            subagent_models: Default::default(),
        },
    )
    .await;
    ok(
        &gateway,
        Command::SetToolPermission {
            thread,
            tool: tool.into(),
            permission: Some(ToolPermission::Allow),
        },
    )
    .await;
    ok(&gateway, Command::PairBroker { pairing }).await;

    // With no search service there is nothing to offer.
    let task = send(&gateway, thread);
    let first = next_request(&mut requests).await;
    assert!(!offered(api, &first.body).contains(&tool.to_owned()));
    first.respond(text_response(api, &nonce())).await;
    finished(task).await;

    let (brave, exa, refused) = (nonce(), nonce(), nonce());
    let mut settings = gateway.snapshot().unwrap().settings;
    settings.search_services = vec![
        SearchConfig {
            base_url: format!("{origin}/res/v1"),
            ..SearchConfig::new(brave.clone(), SearchApi::Brave, names[0].clone())
        },
        SearchConfig {
            base_url: origin.clone(),
            ..SearchConfig::new(exa.clone(), SearchApi::Exa, names[1].clone())
        },
        SearchConfig {
            base_url: origin.clone(),
            ..SearchConfig::new(refused.clone(), SearchApi::Exa, names[2].clone())
        },
    ];
    ok(&gateway, Command::SaveSettings(Box::new(settings))).await;

    // A service alone is not enough: the conversation has no network grant.
    let task = send(&gateway, thread);
    let next = next_request(&mut requests).await;
    assert!(!offered(api, &next.body).contains(&tool.to_owned()));
    next.respond(text_response(api, &nonce())).await;
    finished(task).await;

    ok(
        &gateway,
        Command::SetBackend {
            thread,
            backend: CodingBackend::Native,
            capabilities: [Capability::Network].into(),
        },
    )
    .await;
    let task = send(&gateway, thread);
    let first = next_request(&mut requests).await;
    assert!(offered(api, &first.body).contains(&tool.to_owned()));
    let mut seen = vec![first.body.to_string()];
    let (q1, q2, q3) = (nonce(), nonce(), nonce());
    let calls = [
        Call::new(tool, json!({"query": q1})),
        Call::new(tool, json!({"query": q2, "service": exa})),
        Call::new(tool, json!({"query": q3, "service": refused})),
    ];
    first.respond(calls_response(api, &calls, &nonce())).await;
    let second = next_request(&mut requests).await;
    seen.push(second.body.to_string());
    let read = |call: &Call| {
        let text = result_for(api, &second.body, &call.id).unwrap();
        serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text))
    };
    // The first configured service answers when none is named.
    let by_brave = read(&calls[0]);
    assert_eq!(by_brave["service"], brave);
    assert_eq!(by_brave["results"][0]["url"], result_url(&q1));
    assert_eq!(by_brave["results"][0]["title"], q1);
    let by_exa = read(&calls[1]);
    assert_eq!(by_exa["service"], exa);
    assert_eq!(by_exa["results"][0]["text"], q2);
    second.respond(text_response(api, &nonce())).await;
    let done = finished(task).await;
    assert_eq!(done.state, RunState::Complete);

    // The service refused the third key: the call fails, naming why.
    let records = calls_of(&done);
    let failed = records.iter().find(|c| c.id == calls[2].id).unwrap();
    assert_eq!(failed.state, ToolCallState::Failed);
    assert_eq!(error_kind(failed), ErrorKind::PermissionDenied);
    assert_eq!(*services.asked.lock().unwrap(), [q1, q2]);

    // No key reached the model.
    for body in &seen {
        for key in [&services.brave_key, &services.exa_key, &wrong] {
            assert!(!body.contains(key.as_str()));
        }
    }
}

#[tokio::test]
async fn services_with_bad_endpoints_or_shared_names_are_refused() {
    let state = tempfile::tempdir().unwrap();
    let gateway = Gateway::open(state.path()).unwrap();
    let mut settings = gateway.snapshot().unwrap().settings;
    let label = nonce();
    settings.search_services = vec![
        SearchConfig::new(label.clone(), SearchApi::Brave, nonce()),
        SearchConfig::new(label.to_uppercase(), SearchApi::Exa, nonce()),
    ];
    let reply = request(&gateway, Command::SaveSettings(Box::new(settings.clone()))).await;
    let Reply::Error(report) = reply else {
        panic!("refused: {reply:?}");
    };
    assert_eq!(report.kind, ErrorKind::InvalidRequest);
    settings.search_services = vec![SearchConfig {
        base_url: format!("http://{}.example", nonce()),
        ..SearchConfig::new(label, SearchApi::Exa, nonce())
    }];
    let reply = request(&gateway, Command::SaveSettings(Box::new(settings))).await;
    assert!(
        matches!(reply, Reply::Error(ref r) if r.kind == ErrorKind::InvalidRequest),
        "{reply:?}"
    );
    assert!(
        gateway
            .snapshot()
            .unwrap()
            .settings
            .search_services
            .is_empty()
    );
}
