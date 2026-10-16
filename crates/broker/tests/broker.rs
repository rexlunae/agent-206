#![cfg(unix)]
//! Exercises the real broker server over its socket with independent clients.
use a206_broker::*;
use a206_vault::{AccessPolicy, CredentialKind, CredentialMeta};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

fn random() -> String {
    uuid::Uuid::new_v4().to_string()
}

async fn start() -> (tempfile::TempDir, PathBuf, tokio::task::JoinHandle<()>) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_owned();
    let serving = root.clone();
    let task = tokio::spawn(async move { serve(&serving).await.unwrap() });
    for _ in 0..200 {
        if socket_path(&root).exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (dir, root, task)
}

async fn user(root: &Path) -> Client {
    Client::connect(root, Role::User).await.unwrap()
}

fn unlock(password: &str, totp: Option<String>, create: bool) -> Request {
    Request::Unlock {
        password: WireSecret::new(password.to_owned()),
        totp,
        create,
    }
}

fn meta(destinations: Vec<String>) -> CredentialMeta {
    CredentialMeta {
        label: random(),
        kind: CredentialKind::ApiKey,
        policy: AccessPolicy::WithApproval,
        description: None,
        disabled: false,
        destinations,
    }
}

fn denied(result: Result<Reply, ClientError>) -> BrokerError {
    match result {
        Err(ClientError::Broker(error)) => error,
        other => panic!("expected a broker error, got {other:?}"),
    }
}

async fn status(client: &mut Client) -> BrokerStatus {
    match client.call(Request::Status).await.unwrap() {
        Reply::Status(status) => status,
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn roles_creation_step_up_reveal_and_lock_revocation() {
    let (_dir, root, task) = start().await;
    let password = random();
    let mut a = user(&root).await;
    assert!(!status(&mut a).await.exists);
    assert_eq!(
        denied(a.call(unlock(&password, None, false)).await),
        BrokerError::NoVault
    );
    a.call(unlock(&password, None, true)).await.unwrap();
    let s = status(&mut a).await;
    assert!(s.exists && s.unlocked && s.step_up_active);
    assert_eq!(s.totp_enrolled, Some(false));
    assert_eq!(s.agent_access, Some(false));

    // The gateway role can only use Status and Provider.
    let mut gateway = Client::connect(&root, Role::Gateway).await.unwrap();
    for request in [
        Request::Credentials,
        Request::Reveal { name: random() },
        Request::Lock,
        Request::SetAgentAccess { enabled: true },
    ] {
        assert_eq!(denied(gateway.call(request).await), BrokerError::Forbidden);
    }
    let call = || ProviderCall {
        credential: random(),
        method: HttpMethod::Get,
        url: "https://example.invalid/".into(),
        auth: AuthScheme::Bearer,
        anthropic_version: None,
        body: None,
    };
    assert_eq!(
        denied(a.call(Request::Provider(call())).await),
        BrokerError::Forbidden
    );

    let name = random();
    let value = random();
    let put = || Request::PutCredential {
        name: name.clone(),
        meta: meta(vec![]),
        value: WireValue::Single(WireSecret::new(value.clone())),
        replace: false,
    };
    a.call(put()).await.unwrap();
    // A fresh connection must step up before adding, revealing or changing policy.
    let mut b = user(&root).await;
    assert_eq!(denied(b.call(put()).await), BrokerError::StepUpRequired);
    assert_eq!(
        denied(b.call(Request::Reveal { name: name.clone() }).await),
        BrokerError::StepUpRequired
    );
    assert_eq!(
        denied(
            b.call(Request::UpdateCredential {
                name: name.clone(),
                policy: Some(AccessPolicy::Always),
                disabled: None,
                destinations: None,
            })
            .await
        ),
        BrokerError::StepUpRequired
    );
    assert_eq!(
        denied(
            b.call(Request::StepUp {
                password: Some(WireSecret::new(random())),
                totp: None,
            })
            .await
        ),
        BrokerError::AuthenticationFailed
    );
    b.call(Request::StepUp {
        password: Some(WireSecret::new(password.clone())),
        totp: None,
    })
    .await
    .unwrap();
    // Adding never silently replaces; replacing is explicit.
    assert_eq!(denied(b.call(put()).await), BrokerError::AlreadyExists);
    let mut replacement = put();
    if let Request::PutCredential { replace, .. } = &mut replacement {
        *replace = true;
    }
    b.call(replacement).await.unwrap();
    match b
        .call(Request::Reveal { name: name.clone() })
        .await
        .unwrap()
    {
        Reply::Value(WireValue::Single(v)) => assert_eq!(v.expose(), value),
        other => panic!("unexpected {other:?}"),
    }
    match b.call(Request::Credentials).await.unwrap() {
        Reply::Credentials { typed, legacy } => {
            assert_eq!(typed.len(), 1);
            assert!(legacy.is_empty());
            assert!(!format!("{typed:?}").contains(&value));
        }
        other => panic!("unexpected {other:?}"),
    }

    // Locking from any connection voids every step-up, even after re-unlock.
    a.call(Request::Lock).await.unwrap();
    assert_eq!(
        denied(b.call(Request::Credentials).await),
        BrokerError::Locked
    );
    a.call(unlock(&password, None, false)).await.unwrap();
    assert_eq!(
        denied(b.call(Request::Reveal { name }).await),
        BrokerError::StepUpRequired
    );
    a.call(Request::Shutdown).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert!(!socket_path(&root).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn lockout_is_shared_across_connections_and_malformed_frames_close() {
    let (_dir, root, task) = start().await;
    let password = random();
    user(&root)
        .await
        .call(unlock(&password, None, true))
        .await
        .unwrap();
    user(&root).await.call(Request::Lock).await.unwrap();
    for _ in 0..2 {
        assert_eq!(
            denied(user(&root).await.call(unlock(&random(), None, false)).await),
            BrokerError::AuthenticationFailed
        );
    }
    assert!(matches!(
        denied(user(&root).await.call(unlock(&random(), None, false)).await),
        BrokerError::LockedOut { .. }
    ));
    // Even the right password is refused during the lockout, on a new connection.
    assert!(matches!(
        denied(user(&root).await.call(unlock(&password, None, false)).await),
        BrokerError::LockedOut { .. }
    ));

    // Requests before Hello are refused; garbage frames end the connection.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut raw = tokio::net::UnixStream::connect(socket_path(&root))
        .await
        .unwrap();
    raw.write_all(b"A206\0\x01\xff\xff\xff\xff").await.unwrap();
    let mut buf = [0; 16];
    assert_eq!(raw.read(&mut buf).await.unwrap(), 0);

    // A client speaking another broker version is told so, not misread.
    let mut other = tokio::net::UnixStream::connect(socket_path(&root))
        .await
        .unwrap();
    let id = uuid::Uuid::new_v4();
    let hello = a206_protocol::encode(&a206_protocol::Request {
        id,
        body: Request::Hello {
            role: Role::User,
            version: BROKER_VERSION + 1,
        },
    })
    .unwrap();
    other.write_all(&hello).await.unwrap();
    let mut decoder = a206_protocol::Decoder::default();
    let mut bytes = vec![0; 1024];
    let replies: Vec<a206_protocol::Response<Reply>> = loop {
        let count = other.read(&mut bytes).await.unwrap();
        assert_ne!(count, 0);
        let replies = decoder.feed(&bytes[..count]).unwrap();
        if !replies.is_empty() {
            break replies;
        }
    };
    assert_eq!(replies[0].id, id);
    assert!(matches!(
        replies[0].body,
        Reply::Error(BrokerError::Invalid(InvalidRequest::UnsupportedVersion))
    ));
    // Shutdown needs no greeting, so any client version can stop the broker.
    Client::shutdown(&root).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert!(!socket_path(&root).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn totp_enrollment_then_unlock_requires_a_code() {
    let (_dir, root, _task) = start().await;
    let password = random();
    let mut a = user(&root).await;
    a.call(unlock(&password, None, true)).await.unwrap();
    let mut fresh = user(&root).await;
    assert_eq!(
        denied(fresh.call(Request::BeginTotp { account: random() }).await),
        BrokerError::StepUpRequired
    );
    assert_eq!(
        denied(
            fresh
                .call(Request::ConfirmTotp {
                    code: "000000".into()
                })
                .await
        ),
        BrokerError::NoPendingEnrollment
    );
    let key = match a
        .call(Request::BeginTotp { account: random() })
        .await
        .unwrap()
    {
        Reply::TotpEnrollment {
            otpauth_url,
            manual_key,
            ..
        } => {
            assert!(otpauth_url.expose().starts_with("otpauth://totp/"));
            assert_eq!(format!("{manual_key:?}"), "[REDACTED]");
            manual_key.expose().to_owned()
        }
        other => panic!("unexpected {other:?}"),
    };
    let generator = totp_rs::TOTP::new_unchecked(
        totp_rs::Algorithm::SHA1,
        6,
        1,
        30,
        totp_rs::Secret::Encoded(key).to_bytes().unwrap(),
        None,
        String::new(),
    );
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    };
    let code = generator.generate(now());
    a.call(Request::ConfirmTotp { code: code.clone() })
        .await
        .unwrap();
    a.call(Request::Lock).await.unwrap();
    assert_eq!(
        denied(a.call(unlock(&password, None, false)).await),
        BrokerError::TotpRequired
    );
    assert!(!status(&mut a).await.unlocked);
    // The confirming code cannot be replayed to unlock.
    assert_eq!(
        denied(a.call(unlock(&password, Some(code), false)).await),
        BrokerError::TotpReplayed
    );
    // Wait for a fresh step so the next code is unused.
    let step = now() / 30;
    while now() / 30 == step {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    a.call(unlock(&password, Some(generator.generate(now())), false))
        .await
        .unwrap();
    assert_eq!(status(&mut a).await.totp_enrolled, Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_calls_are_destination_bound_and_value_free() {
    use axum::{Router, http::HeaderMap, routing::get};
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let body = random();
    let app = {
        let seen = seen.clone();
        let body = body.clone();
        Router::new()
            .route(
                "/ok",
                get(move |headers: HeaderMap| {
                    let seen = seen.clone();
                    let body = body.clone();
                    async move {
                        let auth = headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_owned();
                        seen.lock().unwrap().push(auth);
                        body
                    }
                }),
            )
            .route(
                "/fail",
                get(|headers: HeaderMap| async move {
                    // An upstream error that echoes the credential must not be relayed.
                    let echoed = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    (axum::http::StatusCode::UNAUTHORIZED, echoed)
                }),
            )
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let (_dir, root, _task) = start().await;
    let password = random();
    let mut a = user(&root).await;
    a.call(unlock(&password, None, true)).await.unwrap();
    let (bound, unbound, value) = (random(), random(), random());
    for (name, destinations) in [
        (&bound, vec![format!("{origin}/ignored/path")]),
        (&unbound, vec![]),
    ] {
        a.call(Request::PutCredential {
            name: name.clone(),
            meta: meta(destinations),
            value: WireValue::Single(WireSecret::new(value.clone())),
            replace: false,
        })
        .await
        .unwrap();
    }
    let mut gateway = Client::connect(&root, Role::Gateway).await.unwrap();
    let call = |credential: &str, path: &str| {
        Request::Provider(ProviderCall {
            credential: credential.to_owned(),
            method: HttpMethod::Get,
            url: format!("{origin}{path}"),
            auth: AuthScheme::Bearer,
            anthropic_version: None,
            body: None,
        })
    };
    match gateway.call(call(&bound, "/ok")).await.unwrap() {
        Reply::Provider { status, body: got } => {
            assert_eq!(status, 200);
            assert_eq!(got, body.as_bytes());
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(*seen.lock().unwrap(), [format!("Bearer {value}")]);
    match gateway.call(call(&bound, "/fail")).await.unwrap() {
        Reply::Provider { status, body } => {
            assert_eq!(status, 401);
            assert!(body.is_empty());
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(
        denied(gateway.call(call(&unbound, "/ok")).await),
        BrokerError::Denied(a206_vault::Denial::DestinationNotBound)
    );
    // A non-loopback plain-HTTP destination is refused before any lookup.
    let mut plain = call(&bound, "/ok");
    if let Request::Provider(c) = &mut plain {
        c.url = "http://example.invalid/ok".into();
    }
    assert!(matches!(
        denied(gateway.call(plain).await),
        BrokerError::Invalid(InvalidRequest::ProviderUrl)
    ));
    assert_eq!(seen.lock().unwrap().len(), 1);
    // Locked vault: the gateway is told so instead of silently failing.
    a.call(Request::Lock).await.unwrap();
    assert_eq!(
        denied(gateway.call(call(&bound, "/ok")).await),
        BrokerError::Locked
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn lock_cancels_in_flight_sends_and_body_limits_are_structured() {
    use axum::{Router, routing::get};
    let (entered, mut entered_rx) = tokio::sync::mpsc::channel::<()>(1);
    let app = Router::new()
        .route(
            "/slow",
            get(move || {
                let entered = entered.clone();
                async move {
                    entered.send(()).await.unwrap();
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    "late"
                }
            }),
        )
        // Non-ASCII bytes at the limit must still fit one frame.
        .route("/full", get(|| async { vec![0xffu8; MAX_PROVIDER_BODY] }))
        .route("/over", get(|| async { vec![b'x'; MAX_PROVIDER_BODY + 1] }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let (_dir, root, _task) = start().await;
    let password = random();
    let mut a = user(&root).await;
    a.call(unlock(&password, None, true)).await.unwrap();
    let name = random();
    a.call(Request::PutCredential {
        name: name.clone(),
        meta: meta(vec![origin.clone()]),
        value: WireValue::Single(WireSecret::new(random())),
        replace: false,
    })
    .await
    .unwrap();
    let call = |path: &str, body: Option<Vec<u8>>| {
        Request::Provider(ProviderCall {
            credential: name.clone(),
            method: HttpMethod::Get,
            url: format!("{origin}{path}"),
            auth: AuthScheme::Bearer,
            anthropic_version: None,
            body,
        })
    };
    let mut gateway = Client::connect(&root, Role::Gateway).await.unwrap();
    match gateway.call(call("/full", None)).await.unwrap() {
        Reply::Provider { status, body } => {
            assert_eq!(status, 200);
            assert_eq!(body.len(), MAX_PROVIDER_BODY);
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(
        denied(gateway.call(call("/over", None)).await),
        BrokerError::Upstream(UpstreamFailure::TooLarge)
    );
    assert_eq!(
        denied(
            gateway
                .call(call("/full", Some(vec![b'x'; MAX_PROVIDER_BODY + 1])))
                .await
        ),
        BrokerError::Invalid(InvalidRequest::ProviderBody)
    );

    let request = call("/slow", None);
    let slow = tokio::spawn(async move { gateway.call(request).await });
    tokio::time::timeout(Duration::from_secs(10), entered_rx.recv())
        .await
        .unwrap()
        .unwrap();
    a.call(Request::Lock).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), slow)
        .await
        .expect("lock must abandon the in-flight request")
        .unwrap();
    assert_eq!(denied(result), BrokerError::Locked);
}

/// Stands in for a version-1 broker: it refuses everything before a v1
/// `Hello`, and records whether a greeted `Shutdown` arrived.
#[tokio::test(flavor = "multi_thread")]
async fn stop_reaches_an_older_broker_through_its_own_greeting() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_owned();
    std::fs::create_dir_all(root.join("vault")).unwrap();
    let listener = tokio::net::UnixListener::bind(socket_path(&root)).unwrap();
    let stopped = Arc::new(Mutex::new(false));
    let seen = stopped.clone();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut decoder = a206_protocol::Decoder::default();
                let mut greeted = false;
                let mut bytes = vec![0; 8192];
                loop {
                    let count = stream.read(&mut bytes).await.unwrap();
                    if count == 0 {
                        return;
                    }
                    let requests: Vec<a206_protocol::Request<Request>> =
                        decoder.feed(&bytes[..count]).unwrap();
                    for request in requests {
                        let body = match request.body {
                            Request::Hello { version: 1, .. } if !greeted => {
                                greeted = true;
                                Reply::Ok
                            }
                            Request::Hello { .. } => Reply::Error(BrokerError::Invalid(
                                InvalidRequest::UnsupportedVersion,
                            )),
                            Request::Shutdown if greeted => {
                                *seen.lock().unwrap() = true;
                                Reply::Ok
                            }
                            _ => Reply::Error(BrokerError::Forbidden),
                        };
                        let frame = a206_protocol::encode(&a206_protocol::Response {
                            id: request.id,
                            body,
                        })
                        .unwrap();
                        stream.write_all(&frame).await.unwrap();
                    }
                }
            });
        }
    });
    Client::shutdown(&root).await.unwrap();
    assert!(*stopped.lock().unwrap());
}
