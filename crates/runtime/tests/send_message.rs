//! A model posts to a webhook or an allowed messenger chat with
//! `send_message`: only after the user approves that exact call, through the
//! vault broker, which alone holds the webhook URL or bot token. Every
//! identifier, text and key is generated at runtime.
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
type Gateway = a206_runtime::gateway::Gateway;

/// What the webhook service saw, and a webhook the user disconnects through
/// this gateway as soon as the first post arrives.
#[derive(Clone, Default)]
struct Hooks {
    received: Received,
    disconnect: Arc<Mutex<Option<(Gateway, String)>>>,
}

/// A webhook service: records what each key received.
async fn serve_hooks() -> (String, Hooks) {
    let hooks = Hooks::default();
    let router =
        Router::new()
            .route(
                "/api/webhooks/{key}",
                post(
                    |State(hooks): State<Hooks>,
                     Path(key): Path<String>,
                     Json(body): Json<Value>| async move {
                        hooks.received.lock().unwrap().push((key, body));
                        let disconnect = hooks.disconnect.lock().unwrap().take();
                        if let Some((gateway, name)) = disconnect {
                            let reply =
                                request(&gateway, Command::DisconnectMessenger { name }).await;
                            assert_eq!(reply, Reply::Ok);
                        }
                        axum::http::StatusCode::NO_CONTENT
                    },
                ),
            )
            .with_state(hooks.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    // Detached: the server lives as long as the test's runtime.
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (origin, hooks)
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

/// A gateway paired with a broker holding a Discord webhook's URL, and a
/// conversation that permits `send_message`.
struct Setup {
    gateway: Gateway,
    state: tempfile::TempDir,
    hooks: Hooks,
    hook: String,
    key: String,
    requests: tokio::sync::mpsc::Receiver<Opened>,
    thread: Id,
}

async fn setup(api: ProviderApi) -> Setup {
    let (origin, hooks) = serve_hooks().await;
    let key = nonce();
    let (state, credential, pairing) =
        broker(&origin, &format!("{origin}/api/webhooks/{key}")).await;
    let gateway = Gateway::open(state.path()).unwrap();
    let (model, requests) = connect(&gateway, api).await;
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
    let thread = conversation(&gateway, model).await;
    Setup {
        gateway,
        state,
        hooks,
        hook,
        key,
        requests,
        thread,
    }
}

/// A conversation with `model` that permits `send_message`.
async fn conversation(gateway: &Gateway, model: ModelKey) -> Id {
    let Reply::Thread(thread) = request(
        gateway,
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
        gateway,
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
        gateway,
        Command::SetToolPermission {
            thread: thread.id,
            tool: tool.into(),
            permission: Some(ToolPermission::Allow),
        },
    )
    .await;
    assert_eq!(permitted, Reply::Ok);
    thread.id
}

#[tokio::test]
async fn a_model_posts_to_a_webhook_only_after_the_user_approves_and_never_sees_its_url() {
    let api = ProviderApi::OpenAi;
    let Setup {
        gateway,
        state,
        hooks,
        hook,
        key,
        mut requests,
        thread,
    } = setup(api).await;
    let received = hooks.received;
    let tool = a206_runtime::message_tool::SEND;
    let task = send(&gateway, thread);
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
    let asked = approval(&gateway, thread).await;
    assert_eq!(asked.tool, tool);
    assert!(asked.arguments.contains(&refused));
    let resolve = |asked: ApprovalRequest, decision| Command::ResolveApproval {
        thread,
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
    let asked = approval(&gateway, thread).await;
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
    let task = send(&gateway, thread);
    let next = next_request(&mut requests).await;
    assert!(!offered(api, &next.body).contains(&tool.to_owned()));
    next.respond(text_response(api, &nonce())).await;
    assert_eq!(finished(task).await.state, RunState::Complete);
}

#[tokio::test]
async fn a_webhook_withdrawn_mid_message_gets_no_more_of_it() {
    let api = ProviderApi::OpenAi;
    let Setup {
        gateway,
        state: _state,
        hooks,
        hook,
        mut requests,
        thread,
        ..
    } = setup(api).await;
    // The user disconnects the webhook as soon as its first part arrives.
    *hooks.disconnect.lock().unwrap() = Some((gateway.clone(), hook.clone()));
    let tool = a206_runtime::message_tool::SEND;
    let task = send(&gateway, thread);
    let first = next_request(&mut requests).await;
    // Two Discord parts.
    let text: String = (0..100).map(|_| nonce()).collect::<Vec<_>>().join(" ");
    let call = Call::new(tool, json!({"destination": hook, "text": text}));
    first
        .respond(calls_response(api, std::slice::from_ref(&call), &nonce()))
        .await;
    let asked = approval(&gateway, thread).await;
    assert_eq!(
        request(
            &gateway,
            Command::ResolveApproval {
                thread,
                approval: asked.id,
                digest: asked.digest,
                decision: ApprovalDecision::Approve,
            },
        )
        .await,
        Reply::Ok
    );
    let second = next_request(&mut requests).await;
    let result = result_for(api, &second.body, &call.id).unwrap();
    second.respond(text_response(api, &nonce())).await;
    assert_eq!(finished(task).await.state, RunState::Complete);
    assert_eq!(hooks.received.lock().unwrap().len(), 1);
    assert!(result.contains("1 of 2 parts"), "{result}");
}

type Posted = Arc<Mutex<Vec<(String, Value)>>>;

/// What the Bot API stand-in saw, and a messenger the user disconnects
/// through this gateway as soon as the first message arrives.
#[derive(Clone, Default)]
struct Bot {
    posted: Posted,
    disconnect: Arc<Mutex<Option<(Gateway, String)>>>,
}

/// A Telegram Bot API stand-in: names the bot (or refuses its token), has no
/// updates, and records what each sendMessage carried under the path
/// segment holding the token.
async fn serve_bot(token_works: bool) -> (String, Bot) {
    use axum::http::StatusCode;
    use axum::routing::{any, get};
    let bot = Bot::default();
    let me = json!({"ok": true, "result": {"id": std::process::id(), "username": nonce()}});
    let router = Router::new()
        .route(
            "/{bot}/getMe",
            get(move || async move {
                if token_works {
                    (StatusCode::OK, Json(me))
                } else {
                    (StatusCode::UNAUTHORIZED, Json(json!({"ok": false})))
                }
            }),
        )
        .route(
            "/{bot}/getUpdates",
            any(|| async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                Json(json!({"ok": true, "result": []}))
            }),
        )
        .route(
            "/{bot}/sendMessage",
            post(
                |State(bot): State<Bot>, Path(path): Path<String>, Json(body): Json<Value>| async move {
                    bot.posted.lock().unwrap().push((path, body));
                    let disconnect = bot.disconnect.lock().unwrap().take();
                    if let Some((gateway, name)) = disconnect {
                        let reply = request(&gateway, Command::DisconnectMessenger { name }).await;
                        assert_eq!(reply, Reply::Ok);
                    }
                    Json(json!({"ok": true, "result": {"message_id": 1}}))
                },
            ),
        )
        .with_state(bot.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    // Detached: the server lives as long as the test's runtime.
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (origin, bot)
}

/// A gateway paired with a broker holding a Telegram bot token, a running
/// Telegram messenger allowing one chat, and a conversation that permits
/// `send_message`.
struct Chats {
    gateway: Gateway,
    _state: tempfile::TempDir,
    bot: Bot,
    token: String,
    name: String,
    chat: String,
    requests: tokio::sync::mpsc::Receiver<Opened>,
    thread: Id,
}

async fn chats(api: ProviderApi, token_works: bool) -> Chats {
    let (origin, bot) = serve_bot(token_works).await;
    let token = format!("{}:{}", std::process::id(), nonce().replace('-', "_"));
    let (state, credential, pairing) = broker(&origin, &token).await;
    let gateway = Gateway::open(state.path()).unwrap();
    let (model, requests) = connect(&gateway, api).await;
    let (name, chat) = (format!("t-{}", nonce()), nonce());
    let mut settings = gateway.snapshot().unwrap().settings;
    settings.messengers.push(MessengerConfig {
        enabled: true,
        credential: Some(credential),
        endpoint: Some(origin.clone()),
        allowed_chats: [chat.clone()].into(),
        ..MessengerConfig::new(name.clone(), MessengerKind::Telegram)
    });
    assert_eq!(
        request(&gateway, Command::SaveSettings(Box::new(settings))).await,
        Reply::Ok
    );
    assert_eq!(
        request(&gateway, Command::PairBroker { pairing }).await,
        Reply::Ok
    );
    let supervisor = gateway.clone();
    // Detached: the messengers run as long as the test's runtime.
    tokio::spawn(async move { supervisor.supervise_messengers().await });
    // The messenger connects, or fails on its refused token.
    let settled = if token_works {
        MessengerState::Connected
    } else {
        MessengerState::Failed
    };
    tokio::time::timeout(PATIENCE, async {
        while gateway.snapshot().unwrap().messengers[0].state != settled {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(wait("the messenger to settle"));
    let thread = conversation(&gateway, model).await;
    Chats {
        gateway,
        _state: state,
        bot,
        token,
        name,
        chat,
        requests,
        thread,
    }
}

fn approve(thread: Id, asked: ApprovalRequest) -> Command {
    Command::ResolveApproval {
        thread,
        approval: asked.id,
        digest: asked.digest,
        decision: ApprovalDecision::Approve,
    }
}

#[tokio::test]
async fn a_model_posts_only_to_allowed_chats_of_a_running_messenger() {
    let api = ProviderApi::OpenAi;
    let Chats {
        gateway,
        bot,
        token,
        name,
        chat,
        mut requests,
        thread,
        _state,
        ..
    } = chats(api, true).await;
    let other = nonce();
    let tool = a206_runtime::message_tool::SEND;

    // Only the allowed chat is offered.
    let task = send(&gateway, thread);
    let first = next_request(&mut requests).await;
    assert!(offered(api, &first.body).contains(&tool.to_owned()));
    let body = first.body.to_string();
    assert!(body.contains(&format!("{name}/{chat}")));
    assert!(!body.contains(&other));
    let mut seen = vec![body];

    // A chat the user never allowed is refused even once the user approves.
    let stray = Call::new(
        tool,
        json!({"destination": format!("{name}/{other}"), "text": nonce()}),
    );
    first
        .respond(calls_response(api, std::slice::from_ref(&stray), &nonce()))
        .await;
    let asked = approval(&gateway, thread).await;
    assert_eq!(request(&gateway, approve(thread, asked)).await, Reply::Ok);
    let second = next_request(&mut requests).await;
    let refused = result_for(api, &second.body, &stray.id).unwrap();
    assert!(
        refused.contains("not an enabled webhook or an allowed chat"),
        "{refused}"
    );
    seen.push(second.body.to_string());
    assert!(
        bot.posted.lock().unwrap().is_empty(),
        "posted to a chat not allowed"
    );

    // The allowed chat gets the approved text, and only it.
    let text = nonce();
    let sent = Call::new(
        tool,
        json!({"destination": format!("{name}/{chat}"), "text": text}),
    );
    second
        .respond(calls_response(api, std::slice::from_ref(&sent), &nonce()))
        .await;
    let asked = approval(&gateway, thread).await;
    assert!(asked.arguments.contains(&text));
    assert_eq!(request(&gateway, approve(thread, asked)).await, Reply::Ok);
    let third = next_request(&mut requests).await;
    let result = result_for(api, &third.body, &sent.id).unwrap();
    assert!(result.contains("\"parts\":1"), "{result}");
    seen.push(third.body.to_string());
    third.respond(text_response(api, &nonce())).await;
    assert_eq!(finished(task).await.state, RunState::Complete);
    let delivered = bot.posted.lock().unwrap().clone();
    let [(path, body)] = delivered.as_slice() else {
        panic!("one message: {delivered:?}");
    };
    assert_eq!(path, &format!("bot{token}"));
    assert_eq!(body["chat_id"], chat);
    assert_eq!(body["text"], text);
    for body in &seen {
        assert!(!body.contains(&token));
    }

    // Once the user disconnects the messenger, its chats are not offered.
    assert_eq!(
        request(
            &gateway,
            Command::DisconnectMessenger { name: name.clone() }
        )
        .await,
        Reply::Ok
    );
    let task = send(&gateway, thread);
    let next = next_request(&mut requests).await;
    assert!(!offered(api, &next.body).contains(&tool.to_owned()));
    next.respond(text_response(api, &nonce())).await;
    assert_eq!(finished(task).await.state, RunState::Complete);
}

#[tokio::test]
async fn a_messenger_that_failed_to_connect_offers_no_chat() {
    let api = ProviderApi::OpenAi;
    let Chats {
        gateway,
        mut requests,
        thread,
        _state,
        ..
    } = chats(api, false).await;
    let task = send(&gateway, thread);
    let next = next_request(&mut requests).await;
    let tool = a206_runtime::message_tool::SEND;
    assert!(!offered(api, &next.body).contains(&tool.to_owned()));
    next.respond(text_response(api, &nonce())).await;
    assert_eq!(finished(task).await.state, RunState::Complete);
}

#[tokio::test]
async fn a_chat_withdrawn_mid_message_gets_no_more_of_it() {
    let api = ProviderApi::OpenAi;
    let Chats {
        gateway,
        bot,
        name,
        chat,
        mut requests,
        thread,
        _state,
        ..
    } = chats(api, true).await;
    // The user disconnects the messenger as soon as its first part arrives.
    *bot.disconnect.lock().unwrap() = Some((gateway.clone(), name.clone()));
    let tool = a206_runtime::message_tool::SEND;
    let task = send(&gateway, thread);
    let first = next_request(&mut requests).await;
    // Two Telegram parts.
    let text: String = (0..150).map(|_| nonce()).collect::<Vec<_>>().join(" ");
    let call = Call::new(
        tool,
        json!({"destination": format!("{name}/{chat}"), "text": text}),
    );
    first
        .respond(calls_response(api, std::slice::from_ref(&call), &nonce()))
        .await;
    let asked = approval(&gateway, thread).await;
    assert_eq!(request(&gateway, approve(thread, asked)).await, Reply::Ok);
    let second = next_request(&mut requests).await;
    let result = result_for(api, &second.body, &call.id).unwrap();
    second.respond(text_response(api, &nonce())).await;
    assert_eq!(finished(task).await.state, RunState::Complete);
    assert_eq!(bot.posted.lock().unwrap().len(), 1);
    assert!(result.contains("1 of 2 parts"), "{result}");
}

#[tokio::test]
async fn a_test_send_posts_once_to_an_enabled_webhook_without_a_model() {
    let Setup {
        gateway,
        state: _state,
        hooks,
        hook,
        key,
        ..
    } = setup(ProviderApi::OpenAi).await;
    let test = a206_protocol::Request {
        id: Id::new_v4(),
        body: Command::SendTestMessage {
            destination: hook.clone(),
        },
    };
    assert_eq!(gateway.request(test.clone()).await.body, Reply::Ok);
    // A retried request returns its recorded reply instead of posting again.
    assert_eq!(gateway.request(test).await.body, Reply::Ok);
    let delivered = hooks.received.lock().unwrap().clone();
    let [(at, body)] = delivered.as_slice() else {
        panic!("one message: {delivered:?}");
    };
    assert_eq!(at, &key);
    assert_eq!(body["content"], messengers::TEST_MESSAGE);
    assert_eq!(
        gateway.snapshot().unwrap().messengers[0].state,
        MessengerState::Ready
    );

    // A destination that is not configured, or a webhook the user
    // disconnected, is refused and nothing more is sent.
    let refused = |reply: Reply| match reply {
        Reply::Error(report) => assert_eq!(report.kind, ErrorKind::NotFound, "{report:?}"),
        reply => panic!("expected a refusal: {reply:?}"),
    };
    refused(
        request(
            &gateway,
            Command::SendTestMessage {
                destination: nonce(),
            },
        )
        .await,
    );
    assert_eq!(
        request(
            &gateway,
            Command::DisconnectMessenger { name: hook.clone() }
        )
        .await,
        Reply::Ok
    );
    refused(request(&gateway, Command::SendTestMessage { destination: hook }).await);
    assert_eq!(hooks.received.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_webhook_disconnected_while_a_message_goes_out_stays_stopped() {
    let Setup {
        gateway,
        state: _state,
        hooks,
        hook,
        ..
    } = setup(ProviderApi::OpenAi).await;
    let supervisor = gateway.clone();
    // Detached: the messengers run as long as the test's runtime.
    tokio::spawn(async move { supervisor.supervise_messengers().await });
    let state = |gateway: &Gateway| gateway.snapshot().unwrap().messengers[0].state;
    let settled = |want: MessengerState| {
        let gateway = gateway.clone();
        async move {
            tokio::time::timeout(PATIENCE, async {
                while state(&gateway) != want {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap_or_else(wait("the webhook's state"))
        }
    };
    settled(MessengerState::Ready).await;
    // The user disconnects the webhook while its only part is being sent:
    // the message was delivered, but the webhook is no longer ready.
    *hooks.disconnect.lock().unwrap() = Some((gateway.clone(), hook.clone()));
    assert_eq!(
        request(&gateway, Command::SendTestMessage { destination: hook }).await,
        Reply::Ok
    );
    assert_eq!(hooks.received.lock().unwrap().len(), 1);
    settled(MessengerState::Stopped).await;
}

/// Run `job` now, answer its model request with `answer`, and return the run
/// once it has ended and posted its reply.
async fn run_and_answer(setup: &mut Setup, api: ProviderApi, job: Id, answer: &str) -> JobRun {
    let Reply::Job(started) = request(&setup.gateway, Command::RunJob { id: job }).await else {
        panic!("job started");
    };
    let run = started.last.unwrap().id;
    let opened = next_request(&mut setup.requests).await;
    opened.respond(text_response(api, answer)).await;
    tokio::time::timeout(PATIENCE, async {
        loop {
            let state = setup.gateway.snapshot().unwrap();
            let job = state.jobs.iter().find(|j| j.id == job).unwrap();
            match &job.last {
                // A completed run's reply is posted after its end is recorded.
                Some(last)
                    if last.id == run
                        && last.outcome != JobOutcome::Running
                        && last.delivery != Some(Delivery::Posting) =>
                {
                    return last.clone();
                }
                _ => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
    })
    .await
    .unwrap_or_else(wait("the job's run to end"))
}

#[tokio::test]
async fn a_scheduled_message_posts_its_reply_where_the_user_chose() {
    let api = ProviderApi::OpenAi;
    let mut setup = setup(api).await;
    let Reply::Job(job) = request(
        &setup.gateway,
        Command::ScheduleJob {
            kind: JobKind::Message,
            name: nonce(),
            thread: setup.thread,
            text: nonce(),
            schedule: Schedule::Every {
                seconds: 3600,
                anchor: Some(Id::new_v4().as_u128() as u64 % 1_000_000_000),
            },
        },
    )
    .await
    else {
        panic!("job scheduled");
    };
    let deliver = |to: Option<String>| Command::SetJobDelivery { id: job.id, to };
    // Only a configured destination is accepted.
    let Reply::Error(refused) = request(&setup.gateway, deliver(Some(nonce()))).await else {
        panic!("an unknown destination is refused");
    };
    assert_eq!(refused.kind, ErrorKind::InvalidRequest);
    let Reply::Job(set) = request(&setup.gateway, deliver(Some(setup.hook.clone()))).await else {
        panic!("delivery set");
    };
    assert_eq!(set.deliver_to.as_ref(), Some(&setup.hook));

    let answer = nonce();
    let run = run_and_answer(&mut setup, api, job.id, &answer).await;
    assert_eq!(run.outcome, JobOutcome::Complete);
    assert_eq!(run.delivery, Some(Delivery::Sent { parts: 1 }));
    let delivered = setup.hooks.received.lock().unwrap().clone();
    let [(at, body)] = delivered.as_slice() else {
        panic!("one message: {delivered:?}");
    };
    assert_eq!(at, &setup.key);
    assert_eq!(body["content"], answer);

    // A webhook the user disconnected is not posted to; the run still
    // completes and records why its reply was not posted.
    let hook = setup.hook.clone();
    assert_eq!(
        request(&setup.gateway, Command::DisconnectMessenger { name: hook }).await,
        Reply::Ok
    );
    let run = run_and_answer(&mut setup, api, job.id, &nonce()).await;
    assert_eq!(run.outcome, JobOutcome::Complete);
    assert!(matches!(run.delivery, Some(Delivery::Failed(_))), "{run:?}");

    // Without a destination nothing is posted.
    let Reply::Job(cleared) = request(&setup.gateway, deliver(None)).await else {
        panic!("delivery cleared");
    };
    assert_eq!(cleared.deliver_to, None);
    let run = run_and_answer(&mut setup, api, job.id, &nonce()).await;
    assert_eq!(run.delivery, None);
    assert_eq!(setup.hooks.received.lock().unwrap().len(), 1);
}

/// A signal-cli daemon serving `account` on a Unix socket in `dir`; the
/// `send` params it received.
async fn serve_signal(dir: &std::path::Path, account: String) -> (String, Received) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let path = dir.join(format!("{}.sock", &nonce()[..8]));
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let received = Received::default();
    let sends = received.clone();
    // Detached: the daemon lives as long as the test's runtime.
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (account, sends) = (account.clone(), sends.clone());
            // Detached: one connection, ended by the gateway or the runtime.
            tokio::spawn(async move {
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let request: Value = serde_json::from_str(&line).unwrap();
                    let result = match request["method"].as_str() {
                        Some("listAccounts") => json!([{ "number": account }]),
                        Some("subscribeReceive") => json!(0),
                        Some("send") => {
                            sends
                                .lock()
                                .unwrap()
                                .push((account.clone(), request["params"].clone()));
                            json!({ "timestamp": 1 })
                        }
                        method => panic!("unexpected request {method:?}"),
                    };
                    let reply = json!({ "jsonrpc": "2.0", "id": request["id"], "result": result });
                    let mut line = serde_json::to_vec(&reply).unwrap();
                    line.push(b'\n');
                    if writer.write_all(&line).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (format!("unix://{}", path.display()), received)
}

#[tokio::test]
async fn a_test_send_reaches_a_signal_group_through_the_running_session() {
    use base64::Engine;
    let (state, _credential, pairing) = broker("https://unused.invalid", &nonce()).await;
    let account = format!("+1555{}", 1_000_000 + std::process::id() % 1_000_000);
    let (endpoint, sends) = serve_signal(state.path(), account.clone()).await;
    let group = base64::engine::general_purpose::STANDARD.encode(Id::new_v4().as_bytes().repeat(2));
    let name = format!("s-{}", nonce());
    let gateway = Gateway::open(state.path()).unwrap();
    let mut settings = gateway.snapshot().unwrap().settings;
    settings.messengers.push(MessengerConfig {
        enabled: true,
        endpoint: Some(endpoint),
        login: Some(account.clone()),
        allowed_chats: [group.clone()].into(),
        ..MessengerConfig::new(name.clone(), MessengerKind::Signal)
    });
    assert_eq!(
        request(&gateway, Command::SaveSettings(Box::new(settings))).await,
        Reply::Ok
    );
    assert_eq!(
        request(&gateway, Command::PairBroker { pairing }).await,
        Reply::Ok
    );
    let supervisor = gateway.clone();
    // Detached: the messengers run as long as the test's runtime.
    tokio::spawn(async move { supervisor.supervise_messengers().await });
    tokio::time::timeout(PATIENCE, async {
        while gateway.snapshot().unwrap().messengers[0].state != MessengerState::Connected {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(wait("the messenger to connect"));
    let destination = format!("{name}/{group}");
    assert_eq!(
        request(
            &gateway,
            Command::SendTestMessage {
                destination: destination.clone()
            }
        )
        .await,
        Reply::Ok
    );
    let delivered = sends.lock().unwrap().clone();
    let [(_, params)] = delivered.as_slice() else {
        panic!("one message: {delivered:?}");
    };
    assert_eq!(
        params,
        &json!({ "account": account, "groupId": group, "message": messengers::TEST_MESSAGE })
    );

    // A chat that is not allowed, or a messenger the user disconnected, is
    // refused and nothing more is sent.
    let refused = |reply: Reply| match reply {
        Reply::Error(report) => assert_eq!(report.kind, ErrorKind::NotFound, "{report:?}"),
        reply => panic!("expected a refusal: {reply:?}"),
    };
    let other = format!("{name}/{}", nonce());
    refused(request(&gateway, Command::SendTestMessage { destination: other }).await);
    assert_eq!(
        request(&gateway, Command::DisconnectMessenger { name }).await,
        Reply::Ok
    );
    refused(request(&gateway, Command::SendTestMessage { destination }).await);
    assert_eq!(sends.lock().unwrap().len(), 1);
}
