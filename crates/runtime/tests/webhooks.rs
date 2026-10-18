//! A model posts to a webhook messenger with `send_message`: only after the
//! user approves that exact call, through the vault broker, which alone holds
//! the webhook URL. Every identifier, text and key is generated at runtime.
mod support;

use a206_core::*;
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::post,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::*;

type Received = Arc<Mutex<Vec<(String, Value)>>>;

/// A webhook service: records what each key received.
async fn serve_hooks() -> (String, Received) {
    let received = Received::default();
    let router = Router::new()
        .route(
            "/api/webhooks/{key}",
            post(
                |State(received): State<Received>,
                 Path(key): Path<String>,
                 Json(body): Json<Value>| async move {
                    received.lock().unwrap().push((key, body));
                    axum::http::StatusCode::NO_CONTENT
                },
            ),
        )
        .with_state(received.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    // Detached: the server lives as long as the test's runtime.
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (origin, received)
}

/// An unlocked broker in a new profile holding `url` as a credential bound to
/// `origin`; the credential's name and a gateway pairing.
async fn broker(origin: &str, url: &str) -> (tempfile::TempDir, String, BrokerPairing) {
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
    let name = nonce();
    user.call(Request::PutCredential {
        name: name.clone(),
        meta: a206_vault::CredentialMeta {
            label: name.clone(),
            kind: a206_vault::CredentialKind::Token,
            policy: a206_vault::AccessPolicy::WithApproval,
            description: None,
            disabled: false,
            destinations: vec![origin.to_owned()],
        },
        value: WireValue::Single(WireSecret::new(url.to_owned())),
        replace: false,
    })
    .await
    .unwrap();
    let Reply::GatewayPairing { token } = user.call(Request::PairGateway).await.unwrap() else {
        panic!("expected a pairing");
    };
    (dir, name, BrokerPairing::new(token.expose().to_owned()))
}

async fn approval(gateway: &a206_runtime::gateway::Gateway, thread: Id) -> ApprovalRequest {
    tokio::time::timeout(PATIENCE, async {
        loop {
            let state = gateway.snapshot().unwrap();
            let thread = state.threads.iter().find(|t| t.id == thread).unwrap();
            match thread.approvals.first() {
                Some(approval) => return approval.clone(),
                None => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
    })
    .await
    .unwrap_or_else(wait("the approval request"))
}

#[tokio::test]
async fn a_model_posts_to_a_webhook_only_after_the_user_approves_and_never_sees_its_url() {
    let (origin, received) = serve_hooks().await;
    let key = nonce();
    let (state, credential, pairing) =
        broker(&origin, &format!("{origin}/api/webhooks/{key}")).await;
    let gateway = a206_runtime::gateway::Gateway::open(state.path()).unwrap();
    let api = ProviderApi::OpenAi;
    let (model, mut requests) = connect(&gateway, api).await;
    let hook = format!("h-{}", nonce());
    let mut settings = gateway.snapshot().unwrap().settings;
    settings.messengers.push(MessengerConfig {
        enabled: true,
        credential: Some(credential),
        endpoint: Some(origin.clone()),
        format: WebhookFormat::Discord,
        ..MessengerConfig::new(hook.clone(), MessengerKind::Webhook)
    });
    assert_eq!(
        request(&gateway, Command::SaveSettings(Box::new(settings))).await,
        Reply::Ok
    );
    assert_eq!(
        request(&gateway, Command::PairBroker { pairing }).await,
        Reply::Ok
    );
    let Reply::Thread(thread) = request(
        &gateway,
        Command::CreateThread {
            title: nonce(),
            mode: Mode::Chat,
        },
    )
    .await
    else {
        panic!("thread created");
    };
    let configured = request(
        &gateway,
        Command::ConfigureThread {
            id: thread.id,
            mode: Mode::Chat,
            model: Some(model),
            subagent_models: Default::default(),
        },
    )
    .await;
    assert!(matches!(configured, Reply::Thread(_)));
    // Allowing the tool on the conversation still leaves each call to the user.
    let tool = a206_runtime::message_tool::SEND;
    let permitted = request(
        &gateway,
        Command::SetToolPermission {
            thread: thread.id,
            tool: tool.into(),
            permission: Some(ToolPermission::Allow),
        },
    )
    .await;
    assert_eq!(permitted, Reply::Ok);

    let task = send(&gateway, thread.id);
    let first = next_request(&mut requests).await;
    assert!(offered(api, &first.body).contains(&tool.to_owned()));
    assert!(first.body.to_string().contains(&hook));
    let mut seen = vec![first.body.to_string()];
    let call = |text: &str| Call::new(tool, json!({"destination": hook, "text": text}));
    // The user denies the first message: nothing is sent.
    let (refused, text) = (nonce(), nonce());
    let denied_call = call(&refused);
    first
        .respond(calls_response(
            api,
            std::slice::from_ref(&denied_call),
            &nonce(),
        ))
        .await;
    let asked = approval(&gateway, thread.id).await;
    assert_eq!(asked.tool, tool);
    assert!(asked.arguments.contains(&refused));
    let resolve = |asked: ApprovalRequest, decision| Command::ResolveApproval {
        thread: thread.id,
        approval: asked.id,
        digest: asked.digest,
        decision,
    };
    assert_eq!(
        request(&gateway, resolve(asked, ApprovalDecision::Deny)).await,
        Reply::Ok
    );
    let second = next_request(&mut requests).await;
    assert!(result_for(api, &second.body, &denied_call.id).is_some());
    seen.push(second.body.to_string());
    assert!(received.lock().unwrap().is_empty(), "sent without approval");

    // The second is approved, and arrives as Discord content.
    let sent_call = call(&text);
    second
        .respond(calls_response(
            api,
            std::slice::from_ref(&sent_call),
            &nonce(),
        ))
        .await;
    let asked = approval(&gateway, thread.id).await;
    assert!(asked.arguments.contains(&text));
    assert_eq!(
        request(&gateway, resolve(asked, ApprovalDecision::Approve)).await,
        Reply::Ok
    );
    let third = next_request(&mut requests).await;
    seen.push(third.body.to_string());
    let result = result_for(api, &third.body, &sent_call.id).unwrap();
    third.respond(text_response(api, &nonce())).await;
    assert_eq!(finished(task).await.state, RunState::Complete);
    let delivered = received.lock().unwrap().clone();
    let [(at, body)] = delivered.as_slice() else {
        panic!("one message: {delivered:?}");
    };
    assert_eq!(at, &key);
    assert_eq!(body["content"], text);
    assert!(result.contains("\"parts\":1"), "{result}");
    let status = gateway.snapshot().unwrap().messengers;
    assert_eq!(status[0].state, MessengerState::Ready);
    assert!(status[0].failure.is_none());

    // The key never reached the model, the conversation or any profile file.
    for body in &seen {
        assert!(!body.contains(&key));
    }
    let mut pending = vec![state.path().to_owned()];
    while let Some(path) = pending.pop() {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            } else if entry.file_type().unwrap().is_file() {
                let bytes = std::fs::read(entry.path()).unwrap();
                let needle = key.as_bytes();
                assert!(
                    !bytes.windows(needle.len()).any(|w| w == needle),
                    "{} holds the webhook key",
                    entry.path().display()
                );
            }
        }
    }

    // Once the user disconnects it, the webhook is no longer offered.
    assert_eq!(
        request(
            &gateway,
            Command::DisconnectMessenger { name: hook.clone() }
        )
        .await,
        Reply::Ok
    );
    let task = send(&gateway, thread.id);
    let next = next_request(&mut requests).await;
    assert!(!offered(api, &next.body).contains(&tool.to_owned()));
    next.respond(text_response(api, &nonce())).await;
    assert_eq!(finished(task).await.state, RunState::Complete);
}
