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
    // A Unix socket's filesystem entry can appear before bind/listen has
    // finished. Wait for a successful greeting, not just the entry's existence.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match Client::connect(&root, Role::User).await {
                Ok(client) => {
                    drop(client);
                    break;
                }
                Err(ClientError::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("broker startup failed: {error:?}"),
            }
        }
    })
    .await
    .expect("broker did not become ready");
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

/// Mint a pairing on a stepped-up user connection and present it.
async fn paired_gateway(user: &mut Client, root: &Path) -> Client {
    let mut gateway = Client::connect(root, Role::Gateway).await.unwrap();
    gateway
        .call(Request::GatewayAuth {
            token: pairing(user).await,
        })
        .await
        .unwrap();
    gateway
}

async fn pairing(user: &mut Client) -> WireSecret {
    match user.call(Request::PairGateway).await.unwrap() {
        Reply::GatewayPairing { token } => token,
        other => panic!("unexpected {other:?}"),
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
    let mut gateway = paired_gateway(&mut a, &root).await;
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
    let mut gateway = paired_gateway(&mut a, &root).await;
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

    // Revoking the pairing (a replacement, or its anchor closing) abandons
    // in-flight sends just as a lock does.
    a.call(unlock(&password, None, false)).await.unwrap();
    for revoke in ["replace", "close anchor"] {
        let token = pairing(&mut a).await;
        let (anchor, claimed) = present(&root, &token).await;
        claimed.unwrap();
        let (mut gateway, result) = present(&root, &token).await;
        result.unwrap();
        let request = call("/slow", None);
        let slow = tokio::spawn(async move { gateway.call(request).await });
        tokio::time::timeout(Duration::from_secs(10), entered_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let _replacement = if revoke == "replace" {
            Some(pairing(&mut a).await)
        } else {
            None
        };
        drop(anchor);
        let result = tokio::time::timeout(Duration::from_secs(10), slow)
            .await
            .unwrap_or_else(|_| panic!("{revoke} must abandon the in-flight request"))
            .unwrap();
        assert_eq!(denied(result), BrokerError::GatewayNotPaired);
    }
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

#[tokio::test(flavor = "multi_thread")]
async fn only_a_paired_gateway_spends_credentials_and_lock_voids_pairing() {
    let (_dir, root, _task) = start().await;
    let password = random();
    let mut a = user(&root).await;
    a.call(unlock(&password, None, true)).await.unwrap();
    let name = random();
    a.call(Request::PutCredential {
        name: name.clone(),
        meta: meta(vec!["https://example.invalid".into()]),
        value: WireValue::Single(WireSecret::new(random())),
        replace: false,
    })
    .await
    .unwrap();
    // A fragment is refused before any network use, so a paired gateway
    // reaches validation while an unpaired one never does.
    let call = || {
        Request::Provider(ProviderCall {
            credential: name.clone(),
            method: HttpMethod::Get,
            url: "https://example.invalid/#x".into(),
            auth: AuthScheme::Bearer,
            anthropic_version: None,
            body: None,
        })
    };
    let paired = |r: Result<Reply, ClientError>| {
        matches!(denied(r), BrokerError::Invalid(InvalidRequest::ProviderUrl))
    };

    let mut claimed = Client::connect(&root, Role::Gateway).await.unwrap();
    assert_eq!(
        denied(claimed.call(call()).await),
        BrokerError::GatewayNotPaired
    );
    assert_eq!(
        denied(
            claimed
                .call(Request::GatewayAuth {
                    token: WireSecret::new(random()),
                })
                .await
        ),
        BrokerError::GatewayNotPaired
    );
    // Minting needs step-up; presenting is gateway-only.
    let mut fresh = user(&root).await;
    assert_eq!(
        denied(fresh.call(Request::PairGateway).await),
        BrokerError::StepUpRequired
    );
    assert_eq!(status(&mut a).await.gateway_paired, Some(false));
    let token = pairing(&mut a).await;
    // A minted capability is not proof that the gateway received it.
    assert_eq!(status(&mut a).await.gateway_paired, Some(false));
    assert_eq!(
        denied(
            fresh
                .call(Request::GatewayAuth {
                    token: WireSecret::new(token.expose().to_owned()),
                })
                .await
        ),
        BrokerError::Forbidden
    );
    let mut gateway = Client::connect(&root, Role::Gateway).await.unwrap();
    gateway
        .call(Request::GatewayAuth {
            token: WireSecret::new(token.expose().to_owned()),
        })
        .await
        .unwrap();
    assert!(paired(gateway.call(call()).await));
    assert_eq!(status(&mut a).await.gateway_paired, Some(true));

    // A new pairing replaces the old token for new connections.
    let replacement = pairing(&mut a).await;
    let mut stale = Client::connect(&root, Role::Gateway).await.unwrap();
    assert_eq!(
        denied(
            stale
                .call(Request::GatewayAuth {
                    token: WireSecret::new(token.expose().to_owned()),
                })
                .await
        ),
        BrokerError::GatewayNotPaired
    );

    // Locking voids the pairing, even after the vault is unlocked again.
    a.call(Request::Lock).await.unwrap();
    assert_eq!(status(&mut a).await.gateway_paired, None);
    a.call(unlock(&password, None, false)).await.unwrap();
    assert_eq!(status(&mut a).await.gateway_paired, Some(false));
    assert_eq!(
        denied(gateway.call(call()).await),
        BrokerError::GatewayNotPaired
    );
    let mut after = Client::connect(&root, Role::Gateway).await.unwrap();
    assert_eq!(
        denied(
            after
                .call(Request::GatewayAuth {
                    token: WireSecret::new(replacement.expose().to_owned()),
                })
                .await
        ),
        BrokerError::GatewayNotPaired
    );
    let mut repaired = paired_gateway(&mut a, &root).await;
    assert!(paired(repaired.call(call()).await));
}

/// Child-process half of `a_pairing_belongs_to_the_first_process_to_prove_it`;
/// it does nothing unless that test spawned it.
#[tokio::test]
async fn pairing_helper_process() {
    let (Ok(root), Ok(token), Ok(expect)) = (
        std::env::var("A206_PAIR_ROOT"),
        std::env::var("A206_PAIR_TOKEN"),
        std::env::var("A206_PAIR_EXPECT"),
    ) else {
        return;
    };
    let mut gateway = Client::connect(Path::new(&root), Role::Gateway)
        .await
        .unwrap();
    let result = gateway
        .call(Request::GatewayAuth {
            token: WireSecret::new(token),
        })
        .await;
    if expect == "accepted" {
        result.unwrap();
    } else {
        assert_eq!(denied(result), BrokerError::GatewayNotPaired);
    }
}

/// Present `token` from a separate process, which has its own pid.
async fn from_another_process(root: &Path, token: &WireSecret, expect: &str) {
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "pairing_helper_process", "--test-threads=1"])
        .env("A206_PAIR_ROOT", root)
        .env("A206_PAIR_TOKEN", token.expose())
        .env("A206_PAIR_EXPECT", expect);
    let output = command.output().await.unwrap();
    assert!(
        output.status.success(),
        "helper expecting {expect} failed: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

async fn present(root: &Path, token: &WireSecret) -> (Client, Result<Reply, ClientError>) {
    let mut gateway = Client::connect(root, Role::Gateway).await.unwrap();
    let result = gateway
        .call(Request::GatewayAuth {
            token: WireSecret::new(token.expose().to_owned()),
        })
        .await;
    (gateway, result)
}

/// Whether this gateway connection may currently spend credentials: a paired
/// session gets as far as looking up the (unknown) credential.
async fn spends(gateway: &mut Client) -> bool {
    let call = Request::Provider(ProviderCall {
        credential: random(),
        method: HttpMethod::Get,
        url: "https://example.invalid/".into(),
        auth: AuthScheme::Bearer,
        anthropic_version: None,
        body: None,
    });
    match denied(gateway.call(call).await) {
        BrokerError::GatewayNotPaired => false,
        BrokerError::NotFound => true,
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pairing_belongs_to_the_first_process_to_prove_it() {
    let (_dir, root, task) = start().await;
    let mut user = user(&root).await;
    user.call(unlock(&random(), None, true)).await.unwrap();

    // Another process claims the pairing first, then exits: a copy is useless
    // here, both because this pid is not the holder and because it is void.
    let first = pairing(&mut user).await;
    from_another_process(&root, &first, "accepted").await;
    assert_eq!(
        denied(present(&root, &first).await.1),
        BrokerError::GatewayNotPaired
    );

    // A fresh pairing claimed here works on further connections from this
    // process while its anchor stays open, and is refused to any other.
    let second = pairing(&mut user).await;
    let (mut anchor, claimed) = present(&root, &second).await;
    claimed.unwrap();
    let (mut again, result) = present(&root, &second).await;
    result.unwrap();
    assert!(spends(&mut anchor).await && spends(&mut again).await);
    from_another_process(&root, &second, "refused").await;

    // Replacing the pairing stops sessions that proved the old one.
    let third = pairing(&mut user).await;
    assert!(!spends(&mut again).await);
    let (mut holder, claimed) = present(&root, &third).await;
    claimed.unwrap();
    let (mut other, result) = present(&root, &third).await;
    result.unwrap();
    assert!(spends(&mut other).await);

    // Closing the anchor (the gateway exiting) voids the pairing, so a pid
    // reused by another process later finds nothing to claim.
    assert!(spends(&mut holder).await);
    drop(holder);
    let mut gone = false;
    for _ in 0..200 {
        if !spends(&mut other).await {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(gone, "the pairing outlived its anchor connection");
    assert_eq!(
        denied(present(&root, &third).await.1),
        BrokerError::GatewayNotPaired
    );
    drop(anchor);
    task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn importing_a_vault_verifies_it_and_never_replaces_one() {
    let (_dir, root, task) = start().await;
    // An independent password vault, as an existing installation would hold.
    let source_dir = tempfile::tempdir().unwrap();
    let source = source_dir.path().join("secrets.json");
    let password = random();
    let name = random();
    {
        let mut vault = a206_vault::Vault::locked(source.clone());
        vault
            .unlock(&a206_vault::Secret::new(password.clone()))
            .unwrap();
        vault
            .put_credential(
                &name,
                &meta(vec![]),
                a206_vault::CredentialValue::Single(a206_vault::Secret::new(random())),
            )
            .unwrap();
    }
    let original = std::fs::read(&source).unwrap();
    let import = |source: PathBuf, password: &str| Request::ImportVault {
        source,
        password: WireSecret::new(password.to_owned()),
        totp: None,
    };
    let mut a = user(&root).await;
    assert_eq!(
        denied(a.call(import("secrets.json".into(), &password)).await),
        BrokerError::Invalid(InvalidRequest::ImportSource)
    );
    assert_eq!(
        denied(
            a.call(import(source_dir.path().join(random()), &password))
                .await
        ),
        BrokerError::Invalid(InvalidRequest::ImportSource)
    );
    assert_eq!(
        denied(a.call(import(source.clone(), &random())).await),
        BrokerError::AuthenticationFailed
    );
    assert!(!status(&mut a).await.exists);
    a.call(import(source.clone(), &password)).await.unwrap();
    let s = status(&mut a).await;
    assert!(s.exists && s.unlocked && s.step_up_active);
    match a.call(Request::Credentials).await.unwrap() {
        Reply::Credentials { typed, .. } => {
            assert_eq!(
                typed.into_iter().map(|(n, _)| n).collect::<Vec<_>>(),
                [name]
            )
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(
        denied(a.call(import(source.clone(), &password)).await),
        BrokerError::VaultExists
    );
    assert_eq!(std::fs::read(&source).unwrap(), original);
    // The imported vault opens with its own password from a fresh connection.
    a.call(Request::Lock).await.unwrap();
    let mut b = user(&root).await;
    b.call(unlock(&password, None, false)).await.unwrap();
    b.call(Request::Shutdown).await.unwrap();
    task.await.unwrap();
}

/// An upstream whose response body is fed piece by piece from the test, and
/// which reports when the broker abandons it.
struct Upstream {
    origin: String,
    pieces: tokio::sync::mpsc::Receiver<tokio::sync::mpsc::Sender<Vec<u8>>>,
}

async fn upstream() -> Upstream {
    use axum::{Router, body::Body, routing::get};
    let (opened, pieces) = tokio::sync::mpsc::channel(4);
    let app = Router::new()
        .route(
            "/stream",
            get(move || {
                let opened = opened.clone();
                async move {
                    let (sender, receiver) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
                    opened.send(sender).await.unwrap();
                    Body::from_stream(futures_util::stream::unfold(
                        receiver,
                        |mut receiver| async move {
                            let piece = receiver.recv().await?;
                            Some((Ok::<_, std::io::Error>(piece), receiver))
                        },
                    ))
                }
            }),
        )
        .route(
            "/refused",
            get(|| async { (axum::http::StatusCode::TOO_MANY_REQUESTS, random()) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Upstream { origin, pieces }
}

fn stream_call(credential: &str, url: String) -> ProviderCall {
    ProviderCall {
        credential: credential.to_owned(),
        method: HttpMethod::Get,
        url,
        auth: AuthScheme::Bearer,
        anthropic_version: None,
        body: None,
    }
}

/// An unlocked broker holding one credential bound to `origin`.
async fn broker_with_credential(
    origin: &str,
) -> (tempfile::TempDir, PathBuf, Client, String, String) {
    let (dir, root, _task) = start().await;
    let password = random();
    let mut a = user(&root).await;
    a.call(unlock(&password, None, true)).await.unwrap();
    let name = random();
    a.call(Request::PutCredential {
        name: name.clone(),
        meta: meta(vec![origin.to_owned()]),
        value: WireValue::Single(WireSecret::new(random())),
        replace: false,
    })
    .await
    .unwrap();
    (dir, root, a, name, password)
}

/// Wait (bounded) until the upstream body is dropped, which happens only when
/// the broker abandons the request.
async fn abandoned(sender: &tokio::sync::mpsc::Sender<Vec<u8>>, why: &str) {
    tokio::time::timeout(Duration::from_secs(10), sender.closed())
        .await
        .unwrap_or_else(|_| panic!("{why} must abandon the upstream request"));
}

#[tokio::test(flavor = "multi_thread")]
async fn streams_arrive_in_order_in_bounded_chunks_and_errors_are_typed() {
    let mut up = upstream().await;
    let (_dir, root, mut a, name, _) = broker_with_credential(&up.origin).await;
    let mut gateway = paired_gateway(&mut a, &root).await;
    assert_eq!(gateway.version(), BROKER_VERSION);
    let url = format!("{}/stream", up.origin);
    let mut stream = gateway
        .provider_stream(stream_call(&name, url.clone()))
        .await
        .unwrap();
    assert_eq!(stream.status(), 200);
    let feed = up.pieces.recv().await.unwrap();
    // Each piece is relayed before the next exists, so order is observable;
    // a piece larger than a frame is split, never sent as one oversized frame.
    let pieces = [
        random().into_bytes(),
        random().repeat(MAX_STREAM_CHUNK / 8).into_bytes(),
        random().into_bytes(),
    ];
    for piece in &pieces {
        feed.send(piece.clone()).await.unwrap();
        let mut got = vec![];
        while got.len() < piece.len() {
            let chunk = stream.next().await.unwrap().unwrap();
            assert!(!chunk.is_empty() && chunk.len() <= MAX_STREAM_CHUNK);
            got.extend(chunk);
        }
        assert_eq!(&got, piece);
    }
    drop(feed);
    assert_eq!(stream.next().await.unwrap(), None);
    assert_eq!(stream.next().await.unwrap(), None);

    // An unsuccessful status ends the stream without relaying the body.
    let refused = format!("{}/refused", up.origin);
    let mut stream = gateway
        .provider_stream(stream_call(&name, refused))
        .await
        .unwrap();
    assert_eq!(stream.status(), 429);
    assert_eq!(stream.next().await.unwrap(), None);

    // A body over the broker's limit ends with a structured error, and the
    // connection stays usable afterwards.
    let mut stream = gateway
        .provider_stream(stream_call(&name, url.clone()))
        .await
        .unwrap();
    let feed = up.pieces.recv().await.unwrap();
    let mut sent = 0;
    let error = loop {
        if sent <= MAX_PROVIDER_BODY {
            feed.send(vec![b'x'; MAX_STREAM_CHUNK]).await.unwrap();
            sent += MAX_STREAM_CHUNK;
        }
        match stream.next().await {
            Ok(Some(_)) => {}
            Ok(None) => panic!("an oversized body must not end normally"),
            Err(error) => break error,
        }
    };
    assert!(matches!(
        error,
        ClientError::Broker(BrokerError::Upstream(UpstreamFailure::TooLarge))
    ));
    abandoned(&feed, "an oversized body").await;
    assert!(status(&mut gateway).await.unlocked);

    // A user connection cannot stream.
    let refused = a.provider_stream(stream_call(&name, url)).await;
    assert!(matches!(
        refused.map(|_| ()),
        Err(ClientError::Broker(BrokerError::Forbidden))
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn lock_and_disconnect_stop_a_stream_and_its_upstream_request() {
    let mut up = upstream().await;
    let (_dir, root, mut a, name, password) = broker_with_credential(&up.origin).await;
    let url = format!("{}/stream", up.origin);
    let mut gateway = paired_gateway(&mut a, &root).await;
    let mut stream = gateway
        .provider_stream(stream_call(&name, url.clone()))
        .await
        .unwrap();
    let feed = up.pieces.recv().await.unwrap();
    let piece = random().into_bytes();
    feed.send(piece.clone()).await.unwrap();
    assert_eq!(stream.next().await.unwrap(), Some(piece));
    a.call(Request::Lock).await.unwrap();
    assert!(matches!(
        stream.next().await,
        Err(ClientError::Broker(BrokerError::Locked))
    ));
    abandoned(&feed, "a lock").await;
    drop(gateway);

    // Closing a (non-anchor) connection mid-stream abandons its upstream
    // request although the pairing stays valid.
    a.call(unlock(&password, None, false)).await.unwrap();
    let token = pairing(&mut a).await;
    let (mut anchor, claimed) = present(&root, &token).await;
    claimed.unwrap();
    let (mut gateway, result) = present(&root, &token).await;
    result.unwrap();
    let mut stream = gateway
        .provider_stream(stream_call(&name, url))
        .await
        .unwrap();
    let feed = up.pieces.recv().await.unwrap();
    feed.send(random().into_bytes()).await.unwrap();
    stream.next().await.unwrap().unwrap();
    drop(gateway);
    abandoned(&feed, "a closed connection").await;
    assert!(spends(&mut anchor).await);
}

/// A broker double that greets like `version` and answers a stream or status request
/// with `frames`, to check what the client accepts.
async fn scripted_broker(version: u16, frames: Vec<Reply>) -> tempfile::TempDir {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("vault")).unwrap();
    let listener = tokio::net::UnixListener::bind(socket_path(dir.path())).unwrap();
    let frames = Arc::new(Mutex::new(Some(frames)));
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut decoder = a206_protocol::Decoder::default();
        let mut bytes = vec![0; 8192];
        loop {
            let count = stream.read(&mut bytes).await.unwrap();
            if count == 0 {
                return;
            }
            let requests: Vec<a206_protocol::Request<Request>> =
                decoder.feed(&bytes[..count]).unwrap();
            for request in requests {
                let replies = match request.body {
                    Request::Hello { version: v, .. } if v == version => vec![Reply::Ok],
                    Request::Hello { .. } => vec![Reply::Error(BrokerError::Invalid(
                        InvalidRequest::UnsupportedVersion,
                    ))],
                    Request::ProviderStream(_) | Request::Status => {
                        frames.lock().unwrap().take().unwrap()
                    }
                    _ => vec![Reply::Error(BrokerError::Forbidden)],
                };
                for body in replies {
                    let frame = a206_protocol::encode(&a206_protocol::Response {
                        id: request.id,
                        body,
                    })
                    .unwrap();
                    stream.write_all(&frame).await.unwrap();
                }
            }
        }
    });
    dir
}

#[tokio::test]
async fn old_pairing_status_is_unknown_until_the_broker_can_report_a_proven_holder() {
    for version in [7, 8, 9, 10, BROKER_VERSION] {
        let broker = scripted_broker(
            version,
            vec![Reply::Status(BrokerStatus {
                exists: true,
                unlocked: true,
                totp_enrolled: Some(false),
                agent_access: Some(false),
                step_up_active: false,
                gateway_paired: Some(true),
            })],
        )
        .await;
        let mut client = Client::connect(broker.path(), Role::User).await.unwrap();
        let observed = status(&mut client).await;
        assert_eq!(
            observed.gateway_paired,
            if version >= SETUP_CHANNEL_VERSION {
                Some(true)
            } else {
                None
            }
        );
        assert!(observed.exists && observed.unlocked);
    }
}

#[tokio::test]
async fn clients_negotiate_down_and_refuse_malformed_stream_frames() {
    // An older broker is greeted with its own version and never sent a
    // request it cannot decode.
    let older = scripted_broker(MIN_BROKER_VERSION, vec![]).await;
    let mut client = Client::connect(older.path(), Role::Gateway).await.unwrap();
    assert_eq!(client.version(), MIN_BROKER_VERSION);
    let refused = tokio::time::timeout(
        Duration::from_secs(10),
        client.provider_stream(stream_call(&random(), random())),
    )
    .await
    .expect("a stream request was sent to a broker that cannot answer it");
    assert!(matches!(
        refused.map(|_| ()),
        Err(ClientError::StreamUnsupported)
    ));
    let piece = || random().into_bytes();
    let head = || Reply::ProviderHead { status: 200 };
    let chunk = |seq, bytes| Reply::ProviderChunk { seq, bytes };
    let cases = [
        (
            vec![head(), chunk(0, vec![b'x'; MAX_STREAM_CHUNK + 1])],
            StreamViolation::ChunkTooLarge,
        ),
        (vec![head(), chunk(1, piece())], StreamViolation::OutOfOrder),
        (
            vec![head(), chunk(0, piece()), chunk(0, piece())],
            StreamViolation::OutOfOrder,
        ),
        (
            vec![
                head(),
                chunk(0, piece()),
                Reply::ProviderEnd {
                    chunks: 1,
                    bytes: 1,
                },
            ],
            StreamViolation::Length,
        ),
        (vec![head(), Reply::Ok], StreamViolation::Unexpected),
    ];
    for (frames, expected) in cases {
        let broker = scripted_broker(BROKER_VERSION, frames).await;
        let mut client = Client::connect(broker.path(), Role::Gateway).await.unwrap();
        let mut stream = client
            .provider_stream(stream_call(&random(), random()))
            .await
            .unwrap();
        let error = loop {
            // The double sends nothing after its script: waiting for more
            // means the violation was accepted.
            let next = tokio::time::timeout(Duration::from_secs(10), stream.next())
                .await
                .unwrap_or_else(|_| panic!("{expected:?} was accepted"));
            match next {
                Ok(Some(_)) => {}
                Ok(None) => panic!("{expected:?} was accepted"),
                Err(error) => break error,
            }
        };
        assert!(
            matches!(error, ClientError::Stream(v) if v == expected),
            "expected {expected:?}, got {error:?}"
        );
        // A refused stream stays refused.
        assert_eq!(stream.next().await.unwrap(), None);
    }
}

/// A current broker still serves clients that speak the previous version.
#[tokio::test(flavor = "multi_thread")]
async fn brokers_accept_the_previous_protocol_version() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (_dir, root, _task) = start().await;
    for (version, accepted) in [
        (MIN_BROKER_VERSION - 1, false),
        (MIN_BROKER_VERSION, true),
        (BROKER_VERSION + 1, false),
    ] {
        let mut stream = tokio::net::UnixStream::connect(socket_path(&root))
            .await
            .unwrap();
        let id = uuid::Uuid::new_v4();
        let hello = Request::Hello {
            role: Role::Gateway,
            version,
        };
        let frame = a206_protocol::encode(&a206_protocol::Request { id, body: hello }).unwrap();
        stream.write_all(&frame).await.unwrap();
        let mut decoder = a206_protocol::Decoder::default();
        let mut bytes = vec![0; 8192];
        let reply = loop {
            let count = stream.read(&mut bytes).await.unwrap();
            assert_ne!(count, 0);
            let replies: Vec<a206_protocol::Response<Reply>> =
                decoder.feed(&bytes[..count]).unwrap();
            if let Some(reply) = replies.into_iter().next() {
                break reply.body;
            }
        };
        assert_eq!(matches!(reply, Reply::Ok), accepted, "version {version}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn telegram_tokens_go_in_the_path_and_odd_tokens_are_refused() {
    use axum::{Router, extract::Path as UrlPath, http::HeaderMap, routing::get};
    let seen = Arc::new(Mutex::new(Vec::<(String, bool)>::new()));
    let app = {
        let seen = seen.clone();
        Router::new().route(
            "/{bot}/getMe",
            get(move |UrlPath(bot): UrlPath<String>, headers: HeaderMap| {
                let seen = seen.clone();
                async move {
                    let header = headers.contains_key("authorization");
                    seen.lock().unwrap().push((bot, header));
                    "{}"
                }
            }),
        )
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let (_dir, root, _task) = start().await;
    let mut a = user(&root).await;
    a.call(unlock(&random(), None, true)).await.unwrap();
    // Shaped like a real bot token: digits, a colon, then URL-safe text.
    let token = format!("{}:{}", std::process::id(), random().replace('-', "_"));
    let (good, odd) = (random(), random());
    for (name, value) in [(&good, token.clone()), (&odd, format!("{token}/../x"))] {
        a.call(Request::PutCredential {
            name: name.clone(),
            meta: meta(vec![origin.clone()]),
            value: WireValue::Single(WireSecret::new(value)),
            replace: false,
        })
        .await
        .unwrap();
    }
    let mut gateway = paired_gateway(&mut a, &root).await;
    let call = |credential: &str| {
        Request::Provider(ProviderCall {
            credential: credential.to_owned(),
            method: HttpMethod::Get,
            url: format!("{origin}/getMe"),
            auth: AuthScheme::TelegramBot,
            anthropic_version: None,
            body: None,
        })
    };
    assert!(matches!(
        gateway.call(call(&good)).await.unwrap(),
        Reply::Provider { status: 200, .. }
    ));
    assert_eq!(*seen.lock().unwrap(), [(format!("bot{token}"), false)]);
    assert_eq!(
        denied(gateway.call(call(&odd)).await),
        BrokerError::Upstream(UpstreamFailure::InvalidBotToken)
    );
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn webhook_urls_stay_in_the_vault_and_must_stay_on_their_origin() {
    use axum::{
        Router,
        body::Bytes,
        extract::{Path as UrlPath, RawQuery},
        http::HeaderMap,
        routing::post,
    };
    type Seen = (String, Option<String>, bool, Vec<u8>);
    let seen = Arc::new(Mutex::new(Vec::<Seen>::new()));
    let app = {
        let seen = seen.clone();
        Router::new().route(
            "/hooks/{key}",
            post(
                move |UrlPath(key): UrlPath<String>,
                      RawQuery(query): RawQuery,
                      headers: HeaderMap,
                      body: Bytes| {
                    let seen = seen.clone();
                    async move {
                        let header = headers.contains_key("authorization");
                        seen.lock()
                            .unwrap()
                            .push((key, query, header, body.to_vec()));
                        "ok"
                    }
                },
            ),
        )
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // Another origin: the same host on a port nothing listens on.
    let elsewhere = {
        let unused = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", unused.local_addr().unwrap())
    };

    let (_dir, root, _task) = start().await;
    let mut a = user(&root).await;
    a.call(unlock(&random(), None, true)).await.unwrap();
    let (key, query) = (random(), random());
    let (good, away, odd) = (random(), random(), random());
    for (name, value) in [
        (&good, format!("{origin}/hooks/{key}?thread={query}")),
        (&away, format!("{elsewhere}/hooks/{key}")),
        (&odd, random()),
    ] {
        a.call(Request::PutCredential {
            name: name.clone(),
            meta: meta(vec![origin.clone()]),
            value: WireValue::Single(WireSecret::new(value)),
            replace: false,
        })
        .await
        .unwrap();
    }
    let mut gateway = paired_gateway(&mut a, &root).await;
    let body = random().into_bytes();
    let call = |credential: &str, url: String| {
        Request::Provider(ProviderCall {
            credential: credential.to_owned(),
            method: HttpMethod::Post,
            url,
            auth: AuthScheme::WebhookUrl,
            anthropic_version: None,
            body: Some(body.clone()),
        })
    };
    assert!(matches!(
        gateway.call(call(&good, origin.clone())).await.unwrap(),
        Reply::Provider { status: 200, .. }
    ));
    assert_eq!(
        *seen.lock().unwrap(),
        [(
            key.clone(),
            Some(format!("thread={query}")),
            false,
            body.clone()
        )]
    );
    // The gateway names only the origin: a path or query of its own is
    // refused, so it cannot steer where the stored URL leads.
    for url in [format!("{origin}/hooks/{key}"), format!("{origin}/?x=1")] {
        assert_eq!(
            denied(gateway.call(call(&good, url)).await),
            BrokerError::Invalid(InvalidRequest::WebhookPath)
        );
    }
    // A stored URL on another origin, or a value that is no URL, is never
    // sent anywhere.
    for credential in [&away, &odd] {
        assert_eq!(
            denied(gateway.call(call(credential, origin.clone())).await),
            BrokerError::Upstream(UpstreamFailure::InvalidWebhookUrl)
        );
    }
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn put_requests_carry_their_body_and_the_bearer_token() {
    use axum::{Router, body::Bytes, http::HeaderMap, routing::put};
    let seen = Arc::new(Mutex::new(Vec::<(Option<String>, Vec<u8>)>::new()));
    let app = {
        let seen = seen.clone();
        Router::new().route(
            "/send",
            put(move |headers: HeaderMap, body: Bytes| {
                let seen = seen.clone();
                async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    seen.lock().unwrap().push((auth, body.to_vec()));
                    "{}"
                }
            }),
        )
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let (_dir, root, _task) = start().await;
    let mut a = user(&root).await;
    a.call(unlock(&random(), None, true)).await.unwrap();
    let (name, token) = (random(), random());
    a.call(Request::PutCredential {
        name: name.clone(),
        meta: meta(vec![origin.clone()]),
        value: WireValue::Single(WireSecret::new(token.clone())),
        replace: false,
    })
    .await
    .unwrap();
    let mut gateway = paired_gateway(&mut a, &root).await;
    assert!(gateway.version() >= PUT_VERSION);
    let body = format!("{{\"body\":\"{}\"}}", random()).into_bytes();
    let reply = gateway
        .call(Request::Provider(ProviderCall {
            credential: name,
            method: HttpMethod::Put,
            url: format!("{origin}/send"),
            auth: AuthScheme::Bearer,
            anthropic_version: None,
            body: Some(body.clone()),
        }))
        .await
        .unwrap();
    assert!(matches!(reply, Reply::Provider { status: 200, .. }));
    assert_eq!(
        *seen.lock().unwrap(),
        [(Some(format!("Bearer {token}")), body)]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn search_keys_go_in_the_subscription_header_and_ask_for_json() {
    use axum::{Router, extract::RawQuery, http::HeaderMap, routing::get};
    type Seen = Vec<(
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    )>;
    let seen = Arc::new(Mutex::new(Seen::new()));
    let app = {
        let seen = seen.clone();
        Router::new().route(
            "/web/search",
            get(move |headers: HeaderMap, RawQuery(query): RawQuery| {
                let seen = seen.clone();
                async move {
                    let header = |name: &str| {
                        headers
                            .get(name)
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_owned)
                    };
                    seen.lock().unwrap().push((
                        header("x-subscription-token"),
                        header("accept"),
                        header("authorization"),
                        query,
                    ));
                    "{}"
                }
            }),
        )
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let (_dir, root, _task) = start().await;
    let mut a = user(&root).await;
    a.call(unlock(&random(), None, true)).await.unwrap();
    let (name, token, query) = (random(), random(), random());
    a.call(Request::PutCredential {
        name: name.clone(),
        meta: meta(vec![origin.clone()]),
        value: WireValue::Single(WireSecret::new(token.clone())),
        replace: false,
    })
    .await
    .unwrap();
    let mut gateway = paired_gateway(&mut a, &root).await;
    assert!(gateway.version() >= SEARCH_VERSION);
    let reply = gateway
        .call(Request::Provider(ProviderCall {
            credential: name,
            method: HttpMethod::Get,
            url: format!("{origin}/web/search?q={query}"),
            auth: AuthScheme::SubscriptionToken,
            anthropic_version: None,
            body: None,
        }))
        .await
        .unwrap();
    assert!(matches!(reply, Reply::Provider { status: 200, .. }));
    assert_eq!(
        *seen.lock().unwrap(),
        [(
            Some(token),
            Some("application/json".to_owned()),
            None,
            Some(format!("q={query}"))
        )]
    );
}

/// What the stand-in socket services saw.
#[derive(Clone, Default)]
struct SocketService {
    token: String,
    /// Discord's Identify message, then the gateway's reply to the event.
    received: Arc<Mutex<Vec<String>>>,
    close_code: u16,
    origin: Arc<Mutex<String>>,
}

/// Stand-ins for Discord's `/gateway/bot` and Slack's
/// `apps.connections.open`, each naming a loopback socket.
async fn socket_service(token: String) -> SocketService {
    use axum::{
        Json, Router,
        extract::{RawQuery, State, ws::WebSocketUpgrade},
        http::{HeaderMap, StatusCode},
        routing::{any, get, post},
    };
    use serde_json::json;
    fn authorized(headers: &HeaderMap, expected: &str) -> bool {
        headers.get("authorization").and_then(|v| v.to_str().ok()) == Some(expected)
    }
    let service = SocketService {
        token,
        close_code: 4000 + (uuid::Uuid::new_v4().as_u128() % 900) as u16,
        ..SocketService::default()
    };
    let discord_url = |State(s): State<SocketService>, headers: HeaderMap| async move {
        if !authorized(&headers, &format!("Bot {}", s.token)) {
            // An error body echoing the token must never be relayed.
            return (StatusCode::UNAUTHORIZED, Json(json!({"message": s.token})));
        }
        let origin = s.origin.lock().unwrap().replace("http", "ws");
        (
            StatusCode::OK,
            Json(json!({"url": format!("{origin}/discord")})),
        )
    };
    let slack_url = |State(s): State<SocketService>, headers: HeaderMap| async move {
        if !authorized(&headers, &format!("Bearer {}", s.token)) {
            return Json(json!({"ok": false, "error": "invalid_auth"}));
        }
        let origin = s.origin.lock().unwrap().replace("http", "ws");
        Json(json!({"ok": true, "url": format!("{origin}/slack?ticket={}", random())}))
    };
    let discord = |State(s): State<SocketService>, query: RawQuery, upgrade: WebSocketUpgrade| async move {
        assert_eq!(query.0.as_deref(), Some("v=10&encoding=json"));
        upgrade.on_upgrade(move |socket| converse(s, socket, true))
    };
    let slack = |State(s): State<SocketService>, upgrade: WebSocketUpgrade| async move {
        upgrade.on_upgrade(move |socket| converse(s, socket, false))
    };
    let app = Router::new()
        .route("/gateway/bot", get(discord_url))
        .route("/apps.connections.open", post(slack_url))
        .route("/discord", any(discord))
        .route("/slack", any(slack))
        .with_state(service.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    *service.origin.lock().unwrap() = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    service
}

async fn recv_text(socket: &mut axum::extract::ws::WebSocket) -> String {
    loop {
        if let axum::extract::ws::Message::Text(text) = socket.recv().await.unwrap().unwrap() {
            return text.as_str().to_owned();
        }
    }
}

/// Greet, record Discord's Identify, send one event, record the reply, and
/// close with the service's code and a reason that must not be relayed.
async fn converse(service: SocketService, mut socket: axum::extract::ws::WebSocket, discord: bool) {
    use axum::extract::ws::{CloseFrame, Message};
    let hello = if discord {
        r#"{"op":10,"d":{"heartbeat_interval":45000}}"#
    } else {
        r#"{"type":"hello"}"#
    };
    socket.send(Message::text(hello)).await.unwrap();
    if discord {
        let identify = recv_text(&mut socket).await;
        service.received.lock().unwrap().push(identify);
    }
    let event = format!(r#"{{"event":"{}"}}"#, random());
    socket.send(Message::text(event)).await.unwrap();
    let reply = recv_text(&mut socket).await;
    service.received.lock().unwrap().push(reply);
    socket
        .send(Message::Close(Some(CloseFrame {
            code: service.close_code,
            reason: service.token.clone().into(),
        })))
        .await
        .unwrap();
}

fn opened(open: SocketOpen) -> Socket {
    match open {
        SocketOpen::Opened(socket) => socket,
        SocketOpen::Refused { status, code } => panic!("refused: {status} {code:?}"),
    }
}

async fn text(socket: &mut Socket) -> String {
    match tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .unwrap()
        .unwrap()
    {
        SocketEvent::Text(text) => text,
        other => panic!("expected text, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn sockets_identify_in_the_broker_and_relay_both_ways() {
    let token = random();
    let service = socket_service(token.clone()).await;
    let origin = service.origin.lock().unwrap().clone();
    let (_dir, root, _task) = start().await;
    let mut a = user(&root).await;
    a.call(unlock(&random(), None, true)).await.unwrap();
    let name = random();
    a.call(Request::PutCredential {
        name: name.clone(),
        meta: meta(vec![origin.clone()]),
        value: WireValue::Single(WireSecret::new(token.clone())),
        replace: false,
    })
    .await
    .unwrap();
    let token_pairing = pairing(&mut a).await;
    // The first connection to present a pairing anchors it for the test.
    let mut anchor = Client::connect(&root, Role::Gateway).await.unwrap();
    anchor
        .call(Request::GatewayAuth {
            token: WireSecret::new(token_pairing.expose().to_owned()),
        })
        .await
        .unwrap();
    let gateway = || async {
        let mut gateway = Client::connect(&root, Role::Gateway).await.unwrap();
        gateway
            .call(Request::GatewayAuth {
                token: WireSecret::new(token_pairing.expose().to_owned()),
            })
            .await
            .unwrap();
        gateway
    };
    let intents = uuid::Uuid::new_v4().as_u128() as u64 >> 12;
    let call = |path: &str, scheme| SocketCall {
        credential: name.clone(),
        url: format!("{origin}{path}"),
        scheme,
    };

    // Discord: Hello is relayed, then the broker identifies with the token.
    let discord = SocketScheme::DiscordGateway { intents };
    let mut socket = opened(
        gateway()
            .await
            .socket(call("/gateway/bot", discord.clone()))
            .await
            .unwrap(),
    );
    let hello = text(&mut socket).await;
    assert!(hello.contains(r#""op":10"#));
    let event = text(&mut socket).await;
    assert!(!event.contains(&token));
    let reply = random();
    socket.send(reply.clone()).await.unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .unwrap();
    assert_eq!(
        closed.unwrap(),
        SocketEvent::Closed(Some(service.close_code))
    );
    assert!(matches!(socket.next().await, Err(ClientError::Closed)));
    {
        let received = service.received.lock().unwrap();
        let identify: serde_json::Value = serde_json::from_str(&received[0]).unwrap();
        assert_eq!(identify["op"], 2);
        assert_eq!(identify["d"]["token"], token.as_str());
        assert_eq!(identify["d"]["intents"], intents);
        assert_eq!(received[1], reply);
    }

    // Slack: everything is relayed; the ticketed URL stays in the broker.
    service.received.lock().unwrap().clear();
    let slack = SocketScheme::SlackSocketMode;
    let mut socket = opened(
        gateway()
            .await
            .socket(call("/apps.connections.open", slack.clone()))
            .await
            .unwrap(),
    );
    assert_eq!(text(&mut socket).await, r#"{"type":"hello"}"#);
    let event = text(&mut socket).await;
    assert!(event.starts_with(r#"{"event""#));
    let reply = random();
    socket.send(reply.clone()).await.unwrap();
    assert_eq!(
        socket.next().await.unwrap(),
        SocketEvent::Closed(Some(service.close_code))
    );
    assert_eq!(*service.received.lock().unwrap(), [reply]);

    // A token the service refuses: statuses and plain codes, never bodies.
    a.call(Request::PutCredential {
        name: name.clone(),
        meta: meta(vec![origin.clone()]),
        value: WireValue::Single(WireSecret::new(random())),
        replace: true,
    })
    .await
    .unwrap();
    let refused = gateway()
        .await
        .socket(call("/gateway/bot", discord.clone()))
        .await
        .unwrap();
    assert!(matches!(
        refused,
        SocketOpen::Refused {
            status: 401,
            code: None
        }
    ));
    let refused = gateway()
        .await
        .socket(call("/apps.connections.open", slack.clone()))
        .await
        .unwrap();
    assert!(matches!(
        refused,
        SocketOpen::Refused { status: 200, code: Some(code) } if code == "invalid_auth"
    ));

    // Bound to another origin: denied before anything is sent.
    let elsewhere = SocketCall {
        url: "http://127.0.0.1:9/gateway/bot".to_owned(),
        ..call("/gateway/bot", discord.clone())
    };
    let denied_open = gateway().await.socket(elsewhere).await;
    assert!(matches!(
        denied_open.map(|_| ()),
        Err(ClientError::Broker(BrokerError::Denied(_)))
    ));

    // Only a paired gateway opens sockets, and sends need an open one.
    let mut u = user(&root).await;
    let forbidden = u
        .call(Request::SocketOpen(call("/gateway/bot", discord.clone())))
        .await;
    assert_eq!(denied(forbidden), BrokerError::Forbidden);
    let unpaired = Client::connect(&root, Role::Gateway).await.unwrap();
    let unpaired = unpaired.socket(call("/gateway/bot", discord.clone())).await;
    assert!(matches!(
        unpaired.map(|_| ()),
        Err(ClientError::Broker(BrokerError::GatewayNotPaired))
    ));
    let mut g = gateway().await;
    let no_socket = g.call(Request::SocketSend { text: random() }).await;
    assert_eq!(
        denied(no_socket),
        BrokerError::Invalid(InvalidRequest::NoSocket)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn locking_the_vault_ends_a_socket_session() {
    use axum::{
        Json, Router,
        extract::ws::WebSocketUpgrade,
        routing::{any, get},
    };
    // A service that greets and then stays silent.
    let origin = Arc::new(Mutex::new(String::new()));
    let named = origin.clone();
    let app = Router::new()
        .route(
            "/gateway/bot",
            get(move || {
                let url = format!("{}/socket", named.lock().unwrap().replace("http", "ws"));
                async move { Json(serde_json::json!({ "url": url })) }
            }),
        )
        .route(
            "/socket",
            any(|upgrade: WebSocketUpgrade| async move {
                upgrade.on_upgrade(|mut socket| async move {
                    let hello = axum::extract::ws::Message::text(r#"{"op":10}"#);
                    socket.send(hello).await.unwrap();
                    while let Some(Ok(_)) = socket.recv().await {}
                })
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    *origin.lock().unwrap() = format!("http://{}", listener.local_addr().unwrap());
    let origin = origin.lock().unwrap().clone();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let (_dir, root, _task) = start().await;
    let mut a = user(&root).await;
    a.call(unlock(&random(), None, true)).await.unwrap();
    let name = random();
    a.call(Request::PutCredential {
        name: name.clone(),
        meta: meta(vec![origin.clone()]),
        value: WireValue::Single(WireSecret::new(random())),
        replace: false,
    })
    .await
    .unwrap();
    let gateway = paired_gateway(&mut a, &root).await;
    let mut socket = opened(
        gateway
            .socket(SocketCall {
                credential: name,
                url: format!("{origin}/gateway/bot"),
                scheme: SocketScheme::DiscordGateway { intents: 0 },
            })
            .await
            .unwrap(),
    );
    assert!(text(&mut socket).await.contains("10"));
    a.call(Request::Lock).await.unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .expect("a lock left the socket open");
    assert!(matches!(
        ended,
        Err(ClientError::Broker(BrokerError::Locked))
    ));
}

#[tokio::test]
async fn sockets_are_never_requested_from_an_older_broker() {
    let older = scripted_broker(PUT_VERSION, vec![]).await;
    let client = Client::connect(older.path(), Role::Gateway).await.unwrap();
    assert_eq!(client.version(), PUT_VERSION);
    let refused = tokio::time::timeout(
        Duration::from_secs(10),
        client.socket(SocketCall {
            credential: random(),
            url: random(),
            scheme: SocketScheme::SlackSocketMode,
        }),
    )
    .await
    .expect("a socket request was sent to a broker that cannot answer it");
    assert!(matches!(
        refused.map(|_| ()),
        Err(ClientError::SocketUnsupported)
    ));
}

/// A stand-in IRC server on loopback. Each connection negotiates `caps`,
/// accepts SASL PLAIN only with `password`, welcomes the client, delivers
/// one tagged message, records what the client sends afterwards and closes
/// after the client's first message.
struct IrcService {
    port: u16,
    /// Every line each connection received, in order.
    received: Arc<Mutex<Vec<Vec<String>>>>,
}

async fn irc_service(password: String, caps: &'static str) -> IrcService {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let received = Arc::new(Mutex::new(vec![]));
    let log = received.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (password, log) = (password.clone(), log.clone());
            tokio::spawn(async move {
                let index = {
                    let mut log = log.lock().unwrap();
                    log.push(vec![]);
                    log.len() - 1
                };
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();
                let mut nick = String::new();
                while let Ok(Some(line)) = lines.next_line().await {
                    log.lock().unwrap()[index].push(line.clone());
                    let mut send = vec![];
                    if let Some(name) = line.strip_prefix("NICK ") {
                        nick = name.to_owned();
                        // Pings during registration are answered by the broker.
                        send.push(format!("PING :{}", uuid::Uuid::new_v4().simple()));
                    } else if line == "CAP LS 302" {
                        send.push(format!(":irc.test CAP * LS :{caps}"));
                    } else if let Some(request) = line.strip_prefix("CAP REQ :") {
                        send.push(format!(":irc.test CAP * ACK :{request}"));
                    } else if line == "AUTHENTICATE PLAIN" {
                        send.push("AUTHENTICATE +".to_owned());
                    } else if let Some(payload) = line.strip_prefix("AUTHENTICATE ") {
                        use base64::Engine;
                        let plain = base64::engine::general_purpose::STANDARD
                            .decode(payload)
                            .unwrap();
                        let expected = format!("\0{nick}\0{password}");
                        send.push(if plain == expected.as_bytes() {
                            format!(":irc.test 903 {nick} :SASL authentication successful")
                        } else {
                            format!(":irc.test 904 {nick} :SASL authentication failed")
                        });
                    } else if line == "CAP END" {
                        send.push(format!(":irc.test 001 {nick} :Welcome"));
                        send.push(format!(
                            "@account=friend :friend!u@h PRIVMSG {nick} :{}",
                            uuid::Uuid::new_v4()
                        ));
                    } else if line.starts_with("PRIVMSG ") {
                        break;
                    }
                    for line in send {
                        writer
                            .write_all(format!("{line}\r\n").as_bytes())
                            .await
                            .unwrap();
                    }
                }
            });
        }
    });
    IrcService { port, received }
}

#[tokio::test(flavor = "multi_thread")]
async fn irc_sessions_sign_in_with_sasl_in_the_broker_and_relay_lines() {
    let password = random();
    let service = irc_service(password.clone(), "sasl=PLAIN account-tag").await;
    let server = format!("irc://127.0.0.1:{}", service.port);
    let (_dir, root, _task) = start().await;
    let mut a = user(&root).await;
    a.call(unlock(&random(), None, true)).await.unwrap();
    let name = random();
    // Destinations name the server as written; the broker binds its origin.
    a.call(Request::PutCredential {
        name: name.clone(),
        meta: meta(vec![format!("{server}/")]),
        value: WireValue::Single(WireSecret::new(password.clone())),
        replace: false,
    })
    .await
    .unwrap();
    let Reply::Credentials { typed, .. } = a.call(Request::Credentials).await.unwrap() else {
        panic!("expected credentials");
    };
    assert_eq!(typed[0].1.destinations, std::slice::from_ref(&server));
    let token_pairing = pairing(&mut a).await;
    let mut anchor = Client::connect(&root, Role::Gateway).await.unwrap();
    anchor
        .call(Request::GatewayAuth {
            token: WireSecret::new(token_pairing.expose().to_owned()),
        })
        .await
        .unwrap();
    let gateway = || async {
        let mut gateway = Client::connect(&root, Role::Gateway).await.unwrap();
        gateway
            .call(Request::GatewayAuth {
                token: WireSecret::new(token_pairing.expose().to_owned()),
            })
            .await
            .unwrap();
        gateway
    };
    let login = format!("bot{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let call = |login: &str| SocketCall {
        credential: name.clone(),
        url: server.clone(),
        scheme: SocketScheme::IrcSasl {
            login: login.to_owned(),
        },
    };

    // Lines are relayed from the welcome on; sign-in stays in the broker.
    let mut socket = opened(gateway().await.socket(call(&login)).await.unwrap());
    let welcome = text(&mut socket).await;
    assert_eq!(welcome, format!(":irc.test 001 {login} :Welcome"));
    let message = text(&mut socket).await;
    assert!(message.starts_with("@account=friend :friend!u@h PRIVMSG"));
    let reply = format!("PRIVMSG friend :{}", random());
    socket.send(reply.clone()).await.unwrap();
    assert_eq!(socket.next().await.unwrap(), SocketEvent::Closed(None));
    {
        let received = service.received.lock().unwrap();
        let lines = &received[0];
        assert_eq!(
            lines[..3],
            [
                "CAP LS 302".to_owned(),
                format!("NICK {login}"),
                format!("USER {login} 0 * :Agent 206")
            ]
        );
        assert!(lines.iter().any(|l| l.starts_with("PONG :")));
        assert!(lines.contains(&"CAP REQ :sasl account-tag".to_owned()));
        assert_eq!(lines.last(), Some(&reply));
        assert!(!lines.iter().any(|l| l.contains(&password)));
    }

    // A one-line limit: a line ending would let the gateway smuggle a command.
    let socket = opened(gateway().await.socket(call(&login)).await.unwrap());
    let mut socket = socket;
    text(&mut socket).await;
    socket
        .send(format!("PRIVMSG friend :x\r\nQUIT :{}", random()))
        .await
        .unwrap();
    loop {
        match socket.next().await {
            Ok(SocketEvent::Text(_)) => continue,
            Err(ClientError::Broker(error)) => {
                assert_eq!(error, BrokerError::Invalid(InvalidRequest::IrcLine));
                break;
            }
            other => panic!("expected the session to end, got {other:?}"),
        }
    }

    // A wrong password is a SASL refusal, not a session.
    a.call(Request::PutCredential {
        name: name.clone(),
        meta: meta(vec![server.clone()]),
        value: WireValue::Single(WireSecret::new(random())),
        replace: true,
    })
    .await
    .unwrap();
    let refused = gateway().await.socket(call(&login)).await.unwrap();
    assert!(matches!(
        refused,
        SocketOpen::Refused { status: 0, code: Some(code) } if code == "sasl_failed"
    ));

    // The login cannot change a command, and the URL must be an IRC server.
    let bad_login = gateway().await.socket(call(&format!("{login} :x"))).await;
    assert!(matches!(
        bad_login.map(|_| ()),
        Err(ClientError::Broker(BrokerError::Invalid(
            InvalidRequest::IrcLogin
        )))
    ));
    let https = SocketCall {
        url: format!("http://127.0.0.1:{}", service.port),
        ..call(&login)
    };
    assert!(matches!(
        gateway().await.socket(https).await.map(|_| ()),
        Err(ClientError::Broker(BrokerError::Invalid(
            InvalidRequest::IrcServer
        )))
    ));
    let irc_for_web = SocketCall {
        scheme: SocketScheme::SlackSocketMode,
        ..call(&login)
    };
    assert!(matches!(
        gateway().await.socket(irc_for_web).await.map(|_| ()),
        Err(ClientError::Broker(BrokerError::Invalid(
            InvalidRequest::ProviderUrl
        )))
    ));

    // A credential bound to another server is never sent.
    let elsewhere = SocketCall {
        url: "irc://127.0.0.1:9".to_owned(),
        ..call(&login)
    };
    assert!(matches!(
        gateway().await.socket(elsewhere).await.map(|_| ()),
        Err(ClientError::Broker(BrokerError::Denied(_)))
    ));
    assert_eq!(service.received.lock().unwrap().len(), 3);

    // Plain-text IRC is refused off loopback, at binding time too.
    let insecure = a
        .call(Request::PutCredential {
            name: random(),
            meta: meta(vec![format!(
                "irc://{}.test",
                uuid::Uuid::new_v4().simple()
            )]),
            value: WireValue::Single(WireSecret::new(random())),
            replace: false,
        })
        .await;
    assert_eq!(
        denied(insecure),
        BrokerError::Invalid(InvalidRequest::Destination)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn irc_servers_without_account_tags_are_refused() {
    let password = random();
    let service = irc_service(password.clone(), "sasl").await;
    let server = format!("irc://127.0.0.1:{}", service.port);
    let (_dir, root, _task) = start().await;
    let mut a = user(&root).await;
    a.call(unlock(&random(), None, true)).await.unwrap();
    let name = random();
    a.call(Request::PutCredential {
        name: name.clone(),
        meta: meta(vec![server.clone()]),
        value: WireValue::Single(WireSecret::new(password)),
        replace: false,
    })
    .await
    .unwrap();
    let gateway = paired_gateway(&mut a, &root).await;
    let refused = gateway
        .socket(SocketCall {
            credential: name,
            url: server,
            scheme: SocketScheme::IrcSasl {
                login: format!("bot{}", &uuid::Uuid::new_v4().simple().to_string()[..8]),
            },
        })
        .await
        .unwrap();
    assert!(matches!(
        refused,
        SocketOpen::Refused { status: 0, code: Some(code) } if code == "account_tag_unavailable"
    ));
    // Nothing was authenticated.
    let received = service.received.lock().unwrap();
    assert!(!received[0].iter().any(|l| l.starts_with("AUTHENTICATE")));
}
async fn secure_open(root: &Path) -> (Client, a206_vault_channel::Channel) {
    use a206_vault_channel::Handshake;
    let mut relay = Client::connect(root, Role::Relay).await.unwrap();
    let Reply::ChannelKey(key) = relay.call(Request::ChannelKey).await.unwrap() else {
        panic!("missing key");
    };
    let (handshake, first) = Handshake::start(&key).unwrap();
    let Reply::ChannelOpened(response) = relay.call(Request::OpenChannel { first }).await.unwrap()
    else {
        panic!("missing handshake");
    };
    (relay, handshake.finish(&response).unwrap())
}
async fn secure_call(
    client: &mut Client,
    channel: &mut a206_vault_channel::Channel,
    request: a206_vault_channel::Request,
) -> a206_vault_channel::Reply {
    let bytes = channel.seal(&request).unwrap();
    let Reply::ChannelMessage(bytes) = client
        .call(Request::ChannelMessage { bytes })
        .await
        .unwrap()
    else {
        panic!("expected ciphertext");
    };
    channel.open(&bytes).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn encrypted_agent_access_requires_connection_owned_step_up_and_survives_reopen() {
    use a206_vault_channel::{Code, Reply as R, Request as Q, Secret};
    let (_dir, root, task) = start().await;
    let password = random();
    let mut owner = user(&root).await;
    owner.call(unlock(&password, None, true)).await.unwrap();
    let (mut relay, mut channel) = secure_open(&root).await;
    // Another connection's creation step-up cannot authorize this channel.
    for enabled in [true, false] {
        assert!(matches!(
            secure_call(&mut relay, &mut channel, Q::SetAgentAccess { enabled }).await,
            R::Error(error) if error.code == Code::StepUpRequired
        ));
        assert_eq!(status(&mut owner).await.agent_access, Some(false));
    }
    assert!(matches!(
        secure_call(
            &mut relay,
            &mut channel,
            Q::StepUp {
                password: Some(Secret::new(password.clone())),
                totp: None,
            }
        )
        .await,
        R::Ok
    ));
    for enabled in [true, false, true] {
        assert!(matches!(
            secure_call(&mut relay, &mut channel, Q::SetAgentAccess { enabled }).await,
            R::Ok
        ));
        assert_eq!(status(&mut owner).await.agent_access, Some(enabled));
    }
    owner.call(Request::Lock).await.unwrap();
    assert!(matches!(
        secure_call(
            &mut relay,
            &mut channel,
            Q::SetAgentAccess { enabled: false }
        )
        .await,
        R::Error(_)
    ));
    owner.call(unlock(&password, None, false)).await.unwrap();
    assert_eq!(status(&mut owner).await.agent_access, Some(true));
    // Reopening does not restore this channel's revoked step-up window.
    assert!(matches!(
        secure_call(&mut relay, &mut channel, Q::SetAgentAccess { enabled: false }).await,
        R::Error(error) if error.code == Code::StepUpRequired
    ));
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn encrypted_setup_keeps_seeds_private_and_cancellation_discards_them() {
    use a206_vault_channel::{Code, Reply as R, Request as Q, Secret};
    let (_dir, root, task) = start().await;
    let (mut relay, mut channel) = secure_open(&root).await;
    assert_eq!(
        denied(relay.call(Request::Status).await),
        BrokerError::Forbidden
    );
    assert_eq!(
        denied(relay.call(unlock(&random(), None, true)).await),
        BrokerError::Forbidden
    );
    let password = random();
    assert!(matches!(
        secure_call(
            &mut relay,
            &mut channel,
            Q::Unlock {
                password: Secret::new(password.clone()),
                totp: None,
                create: true
            }
        )
        .await,
        R::Ok
    ));
    assert!(matches!(
        secure_call(&mut relay, &mut channel, Q::BeginTotp { account: random() }).await,
        R::TotpEnrollment { .. }
    ));
    assert!(matches!(
        secure_call(&mut relay, &mut channel, Q::CancelTotp).await,
        R::Ok
    ));
    assert!(matches!(
        secure_call(
            &mut relay,
            &mut channel,
            Q::ConfirmTotp {
                code: Secret::new("000000".into())
            }
        )
        .await,
        R::Error(_)
    ));
    let R::TotpEnrollment { manual_key, .. } =
        secure_call(&mut relay, &mut channel, Q::BeginTotp { account: random() }).await
    else {
        panic!("enrollment missing");
    };
    let generator = totp_rs::TOTP::new_unchecked(
        totp_rs::Algorithm::SHA1,
        6,
        1,
        30,
        totp_rs::Secret::Encoded(manual_key.expose().to_owned())
            .to_bytes()
            .unwrap(),
        None,
        String::new(),
    );
    let mut code = generator.generate(a206_vault::unix_now());
    for attempt in 0..3 {
        let reply = secure_call(
            &mut relay,
            &mut channel,
            Q::ConfirmTotp {
                code: Secret::new(code.clone()),
            },
        )
        .await;
        if matches!(reply, R::Ok) {
            break;
        }
        let fresh = generator.generate(a206_vault::unix_now());
        // Retry only a genuinely expired time slice, never a stable rejection.
        assert!(
            attempt < 2
                && fresh != code
                && matches!(reply,
            R::Error(ref error) if error.code == Code::AuthenticationFailed
                || error.code == Code::ClockDrift),
            "{reply:?}"
        );
        code = fresh;
    }
    assert!(
        matches!(secure_call(&mut relay, &mut channel, Q::Status).await, R::Status(s) if s.totp_enrolled == Some(true))
    );
    assert!(matches!(
        secure_call(&mut relay, &mut channel, Q::Lock).await,
        R::Ok
    ));
    assert!(
        matches!(secure_call(&mut relay, &mut channel, Q::Unlock { password: Secret::new(password.clone()), totp: None, create: false }).await, R::Error(e) if e.code == Code::TotpRequired)
    );
    assert!(
        matches!(secure_call(&mut relay, &mut channel, Q::Unlock { password: Secret::new(password), totp: Some(Secret::new(code)), create: false }).await, R::Error(e) if e.code == Code::TotpReplayed)
    );
    let mut owner = user(&root).await;
    owner.call(Request::Shutdown).await.unwrap();
    task.await.unwrap();
}
#[tokio::test]
async fn relay_cannot_replace_an_authenticated_channel_or_replay_messages() {
    use a206_vault_channel::{Handshake, Request as Q, Secret};
    let (_dir, root, task) = start().await;
    let (mut relay, mut channel) = secure_open(&root).await;
    secure_call(
        &mut relay,
        &mut channel,
        Q::Unlock {
            password: Secret::new(random()),
            totp: None,
            create: true,
        },
    )
    .await;
    let bytes = channel.seal(&Q::Status).unwrap();
    relay
        .call(Request::ChannelMessage {
            bytes: bytes.clone(),
        })
        .await
        .unwrap();
    assert!(matches!(
        denied(relay.call(Request::ChannelMessage { bytes }).await),
        BrokerError::Channel(_)
    ));
    let Reply::ChannelKey(key) = relay.call(Request::ChannelKey).await.unwrap() else {
        panic!();
    };
    let (_, first) = Handshake::start(&key).unwrap();
    assert_eq!(
        denied(relay.call(Request::OpenChannel { first }).await),
        BrokerError::Forbidden
    );
    assert_eq!(
        denied(relay.call(Request::PairGateway).await),
        BrokerError::Forbidden
    );
    let (mut fresh, mut other) = secure_open(&root).await;
    assert!(
        matches!(secure_call(&mut fresh, &mut other, Q::PairGateway).await, a206_vault_channel::Reply::Error(e) if e.code == a206_vault_channel::Code::StepUpRequired)
    );
    user(&root).await.call(Request::Shutdown).await.unwrap();
    task.await.unwrap();
}

fn migration_source() -> (tempfile::TempDir, PathBuf, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secrets.json");
    let password = random();
    let value = random();
    let mut source = a206_vault::Vault::locked(path.clone());
    source
        .unlock(&a206_vault::Secret::new(password.clone()))
        .unwrap();
    let mut policy = meta(vec![]);
    policy.policy = AccessPolicy::SkillOnly(vec![random()]);
    policy.disabled = true;
    source
        .put_credential(
            &random(),
            &policy,
            a206_vault::CredentialValue::Single(a206_vault::Secret::new(value.clone())),
        )
        .unwrap();
    (dir, path, password, value)
}
fn migration_preview(source: PathBuf) -> Request {
    Request::ReviewVaultImport {
        version: a206_vault_channel::IMPORT_REVIEW_VERSION,
        source,
    }
}
fn migration_apply(review: uuid::Uuid, password: &str) -> Request {
    Request::ApplyVaultImport {
        version: a206_vault_channel::IMPORT_REVIEW_VERSION,
        review,
        password: WireSecret::new(password.into()),
        totp: None,
    }
}
async fn migration_review(client: &mut Client, source: &Path) -> a206_vault_channel::ImportReview {
    match client
        .call(migration_preview(source.to_owned()))
        .await
        .unwrap()
    {
        Reply::VaultImportReview(review) => review,
        other => panic!("missing migration review: {other:?}"),
    }
}

#[tokio::test]
async fn reviewed_migration_is_connection_bound_revocable_and_source_guarded() {
    let (_dir, root, task) = start().await;
    let (_source_dir, source, password, value) = migration_source();
    let original = std::fs::read(&source).unwrap();
    let mut a = user(&root).await;
    let mut b = user(&root).await;
    let mut gateway = Client::connect(&root, Role::Gateway).await.unwrap();
    let mut relay = Client::connect(&root, Role::Relay).await.unwrap();
    assert_eq!(
        denied(
            a.call(Request::ReviewVaultImport {
                version: a206_vault_channel::IMPORT_REVIEW_VERSION + 1,
                source: source.clone()
            })
            .await
        ),
        BrokerError::Invalid(InvalidRequest::UnsupportedVersion)
    );
    assert_eq!(
        denied(a.call(migration_preview("relative.json".into())).await),
        BrokerError::Invalid(InvalidRequest::ImportSource)
    );
    assert_eq!(
        denied(gateway.call(migration_preview(source.clone())).await),
        BrokerError::Forbidden
    );
    assert_eq!(
        denied(relay.call(migration_preview(source.clone())).await),
        BrokerError::Forbidden
    );
    let first = migration_review(&mut a, &source).await;
    assert_eq!(first.bytes, original.len() as u64);
    assert!(!vault_path(&root).exists());
    assert_eq!(std::fs::read(&source).unwrap(), original);
    assert_eq!(
        denied(b.call(migration_apply(first.id, &password)).await),
        BrokerError::ImportReviewUnavailable
    );
    let second = migration_review(&mut a, &source).await;
    assert_eq!(
        denied(a.call(migration_apply(first.id, &password)).await),
        BrokerError::ImportReviewUnavailable
    );
    a.call(Request::CancelVaultImport {
        version: a206_vault_channel::IMPORT_REVIEW_VERSION,
    })
    .await
    .unwrap();
    assert_eq!(
        denied(a.call(migration_apply(second.id, &password)).await),
        BrokerError::ImportReviewUnavailable
    );
    let review = migration_review(&mut a, &source).await;
    let mut changed = original.clone();
    changed[0] ^= 1;
    std::fs::write(&source, &changed).unwrap();
    assert_eq!(
        denied(a.call(migration_apply(review.id, &password)).await),
        BrokerError::ImportSourceChanged
    );
    assert!(!vault_path(&root).exists());
    assert_eq!(std::fs::read(&source).unwrap(), changed);
    assert_eq!(
        denied(a.call(migration_apply(review.id, &password)).await),
        BrokerError::ImportReviewUnavailable
    );
    std::fs::write(&source, &original).unwrap();
    let review = migration_review(&mut a, &source).await;
    let encoded = a206_protocol::encode(&a206_protocol::Response {
        id: uuid::Uuid::new_v4(),
        body: Reply::VaultImportReview(review.clone()),
    })
    .unwrap();
    assert!(
        !encoded
            .windows(value.len())
            .any(|part| part == value.as_bytes())
    );
    assert!(!format!("{:?}", migration_apply(review.id, &password)).contains(&password));
    assert_eq!(
        denied(a.call(migration_apply(review.id, &random())).await),
        BrokerError::AuthenticationFailed
    );
    let Reply::VaultImportApplied(applied) =
        a.call(migration_apply(review.id, &password)).await.unwrap()
    else {
        panic!("missing confirmed import")
    };
    assert_eq!(applied.review, review);
    assert!(applied.receipt_error.is_none());
    let path = applied.receipt.unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let receipt: toml::Value = toml::from_str(&text).unwrap();
    assert_eq!(receipt["source"].as_str(), source.to_str());
    assert_eq!(receipt["id"].as_str(), Some(review.id.to_string().as_str()));
    assert!(!text.contains(&password));
    assert!(!text.contains(&value));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        denied(a.call(migration_apply(review.id, &password)).await),
        BrokerError::ImportReviewUnavailable
    );
    assert_eq!(
        denied(a.call(migration_preview(source.clone())).await),
        BrokerError::VaultExists
    );
    assert_eq!(std::fs::read(vault_path(&root)).unwrap(), original);
    assert_eq!(std::fs::read(&source).unwrap(), original);
    let Reply::Credentials { typed, .. } = a.call(Request::Credentials).await.unwrap() else {
        panic!()
    };
    assert!(typed[0].1.disabled);
    assert!(matches!(typed[0].1.policy, AccessPolicy::SkillOnly(_)));
    assert_eq!(status(&mut a).await.agent_access, Some(false));
    a.call(Request::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn reviewed_migration_preserves_an_enrolled_factor_requirement() {
    let (_dir, root, task) = start().await;
    let (_source_dir, source, password, _) = migration_source();
    let mut original = a206_vault::Vault::locked(source.clone());
    original
        .unlock(&a206_vault::Secret::new(password.clone()))
        .unwrap();
    let now = 1_800_000_015;
    let pending = original.begin_totp_enrollment(&random(), now).unwrap();
    let generator = totp_rs::TOTP::new_unchecked(
        totp_rs::Algorithm::SHA1,
        6,
        1,
        30,
        totp_rs::Secret::Encoded(pending.manual_key().expose(str::to_owned))
            .to_bytes()
            .unwrap(),
        None,
        String::new(),
    );
    original
        .confirm_totp_enrollment(&pending, &generator.generate(now), now)
        .unwrap();
    original.lock();
    let before = std::fs::read(&source).unwrap();
    let mut a = user(&root).await;
    let review = migration_review(&mut a, &source).await;
    assert_eq!(
        denied(a.call(migration_apply(review.id, &password)).await),
        BrokerError::TotpRequired
    );
    assert!(!vault_path(&root).exists());
    assert_eq!(std::fs::read(&source).unwrap(), before);
    a.call(Request::Lock).await.unwrap();
    assert_eq!(
        denied(a.call(migration_apply(review.id, &password)).await),
        BrokerError::ImportReviewUnavailable
    );
    a.call(Request::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn encrypted_reviewed_migration_checks_sources_without_exposing_credentials() {
    use a206_vault_channel::{
        Code, IMPORT_REVIEW_VERSION as VERSION, Reply as R, Request as Q, Secret,
    };
    let (_dir, root, task) = start().await;
    let (_source_dir, source, password, _) = migration_source();
    let original = std::fs::read(&source).unwrap();
    let (mut relay, mut channel) = secure_open(&root).await;
    let preview = || Q::ReviewVaultImport {
        version: VERSION,
        source: source.clone(),
    };
    let apply = |review| Q::ApplyVaultImport {
        version: VERSION,
        review,
        password: Secret::new(password.clone()),
        totp: None,
    };
    let R::VaultImportReview(review) = secure_call(&mut relay, &mut channel, preview()).await
    else {
        panic!()
    };
    let mut changed = original.clone();
    changed[0] ^= 1;
    std::fs::write(&source, changed).unwrap();
    let R::Error(error) = secure_call(&mut relay, &mut channel, apply(review.id)).await else {
        panic!()
    };
    assert_eq!(error.code, Code::ImportSourceChanged);
    assert_eq!(error.report.kind, a206_core::ErrorKind::Conflict);
    assert!(error.report.correlation.is_some());
    assert!(!format!("{error:?}").contains(&password));
    assert!(!vault_path(&root).exists());
    std::fs::write(&source, &original).unwrap();
    let R::VaultImportReview(review) = secure_call(&mut relay, &mut channel, preview()).await
    else {
        panic!()
    };
    let bytes = channel.seal(&apply(review.id)).unwrap();
    assert!(
        !bytes
            .windows(password.len())
            .any(|part| part == password.as_bytes())
    );
    let Reply::ChannelMessage(bytes) = relay.call(Request::ChannelMessage { bytes }).await.unwrap()
    else {
        panic!()
    };
    assert!(
        matches!(channel.open::<R>(&bytes).unwrap(), R::VaultImportApplied(applied) if applied.review == review && applied.receipt_error.is_none())
    );
    assert_eq!(std::fs::read(&source).unwrap(), original);
    assert_eq!(std::fs::read(vault_path(&root)).unwrap(), original);
    user(&root).await.call(Request::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn reviewed_migration_refuses_older_peers_before_sending() {
    let broker = scripted_broker(REVIEWED_IMPORT_VERSION - 1, vec![Reply::Ok]).await;
    let mut a = user(broker.path()).await;
    for request in [
        migration_preview("/source".into()),
        migration_apply(uuid::Uuid::new_v4(), &random()),
        Request::CancelVaultImport {
            version: a206_vault_channel::IMPORT_REVIEW_VERSION,
        },
    ] {
        assert!(matches!(
            a.call(request).await,
            Err(ClientError::ReviewedImportUnsupported)
        ));
    }
    assert!(matches!(a.call(Request::Status).await.unwrap(), Reply::Ok));
}

#[tokio::test]
async fn reviewed_migration_cannot_replace_a_target_created_after_review() {
    let (_dir, root, task) = start().await;
    let (_source_dir, source, password, _) = migration_source();
    let source_before = std::fs::read(&source).unwrap();
    let mut a = user(&root).await;
    let mut b = user(&root).await;
    let review = migration_review(&mut a, &source).await;
    b.call(unlock(&random(), None, true)).await.unwrap();
    let target_before = std::fs::read(vault_path(&root)).unwrap();
    assert_eq!(
        denied(a.call(migration_apply(review.id, &password)).await),
        BrokerError::VaultExists
    );
    assert_eq!(
        denied(a.call(Request::PairGateway).await),
        BrokerError::StepUpRequired
    );
    assert_eq!(std::fs::read(vault_path(&root)).unwrap(), target_before);
    assert_eq!(std::fs::read(&source).unwrap(), source_before);
    a.call(Request::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn reviewed_migration_reports_confirmed_copy_and_failed_receipt_separately() {
    let (_dir, root, task) = start().await;
    let (_source_dir, source, password, _) = migration_source();
    let original = std::fs::read(&source).unwrap();
    let mut a = user(&root).await;
    let review = migration_review(&mut a, &source).await;
    std::fs::write(root.join("vault/imports"), "block receipt directory").unwrap();
    let Reply::VaultImportApplied(applied) =
        a.call(migration_apply(review.id, &password)).await.unwrap()
    else {
        panic!("partial result missing")
    };
    assert_eq!(applied.review, review);
    assert!(applied.receipt.is_none());
    let error = applied.receipt_error.unwrap();
    assert_eq!(error.kind, a206_core::ErrorKind::Storage);
    assert_eq!(error.correlation, Some(review.id));
    assert!(error.causes().count() >= 2);
    assert!(status(&mut a).await.unlocked);
    assert_eq!(std::fs::read(vault_path(&root)).unwrap(), original);
    assert_eq!(std::fs::read(&source).unwrap(), original);
    a.call(Request::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn git_calls_send_the_token_as_basic_auth_to_the_service_they_name() {
    use axum::{
        Router,
        body::Bytes,
        extract::{Path, RawQuery},
        http::{HeaderMap, StatusCode, header},
        routing::{get, post},
    };
    use base64::Engine as _;
    type Seen = Arc<Mutex<Vec<(String, Option<String>, String, String, Vec<u8>)>>>;
    let seen: Seen = Arc::default();
    let (advertised, answered) = (random().into_bytes(), random().into_bytes());
    let record = |seen: &Seen, path: &str, query: Option<String>, headers: &HeaderMap, body| {
        let get = |name| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned()
        };
        seen.lock().unwrap().push((
            format!("{path} {}", get(header::ACCEPT)),
            query,
            get(header::AUTHORIZATION),
            get(header::CONTENT_TYPE),
            body,
        ));
    };
    let app = {
        let (a, b) = (seen.clone(), seen.clone());
        let (advertised, answered) = (advertised.clone(), answered.clone());
        Router::new()
            .route(
                "/team/repo.git/info/refs",
                get(move |RawQuery(query): RawQuery, headers: HeaderMap| {
                    // The advertisement of the service asked for.
                    let service = query
                        .as_deref()
                        .and_then(|q| q.strip_prefix("service="))
                        .unwrap_or_default()
                        .to_owned();
                    record(&a, "refs", query, &headers, vec![]);
                    let advertised = advertised.clone();
                    async move {
                        (
                            [(
                                header::CONTENT_TYPE,
                                format!("application/x-{service}-advertisement"),
                            )],
                            advertised,
                        )
                    }
                }),
            )
            .route(
                "/team/repo.git/{service}",
                post(
                    move |Path(service): Path<String>, headers: HeaderMap, body: Bytes| {
                        record(&b, "pack", None, &headers, body.to_vec());
                        let answered = answered.clone();
                        async move {
                            (
                                [(
                                    header::CONTENT_TYPE,
                                    format!("application/x-{service}-result"),
                                )],
                                answered,
                            )
                        }
                    },
                ),
            )
            // Answers every service with a push's advertisement.
            .route(
                "/mixed/info/refs",
                get(|| async {
                    (
                        [(
                            header::CONTENT_TYPE,
                            "application/x-git-receive-pack-advertisement",
                        )],
                        random(),
                    )
                }),
            )
            // A sign-in page answers with HTML, not git.
            .route(
                "/login/info/refs",
                get(|| async { ([(header::CONTENT_TYPE, "text/html")], random()) }),
            )
            .route(
                "/denied/info/refs",
                get(|| async { (StatusCode::FORBIDDEN, random()) }),
            )
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let (_dir, root, _task) = start().await;
    let mut a = user(&root).await;
    a.call(unlock(&random(), None, true)).await.unwrap();
    let (bound, elsewhere, token, username) = (random(), random(), random(), random());
    for (name, destination) in [
        (&bound, origin.clone()),
        (&elsewhere, format!("https://{}.invalid", random())),
    ] {
        a.call(Request::PutCredential {
            name: name.clone(),
            meta: meta(vec![destination]),
            value: WireValue::Single(WireSecret::new(token.clone())),
            replace: false,
        })
        .await
        .unwrap();
    }
    let call = |credential: &str, repository: String, request: Option<Vec<u8>>| {
        Request::GitReceivePack(GitCall {
            credential: credential.to_owned(),
            username: username.clone(),
            repository,
            request,
        })
    };
    let repository = format!("{origin}/team/repo.git/");
    // Only a paired gateway pushes.
    assert_eq!(
        denied(a.call(call(&bound, repository.clone(), None)).await),
        BrokerError::Forbidden
    );
    let mut gateway = paired_gateway(&mut a, &root).await;
    let reply = |reply| match reply {
        Reply::Provider { status, body } => (status, body),
        other => panic!("unexpected {other:?}"),
    };
    let advertisement = gateway
        .call(call(&bound, repository.clone(), None))
        .await
        .unwrap();
    assert_eq!(reply(advertisement), (200, advertised.clone()));
    let pack = random().into_bytes();
    let result = gateway
        .call(call(&bound, repository.clone(), Some(pack.clone())))
        .await
        .unwrap();
    assert_eq!(reply(result), (200, answered.clone()));
    let basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{token}"))
    );
    assert_eq!(
        *seen.lock().unwrap(),
        [
            (
                "refs application/x-git-receive-pack-advertisement".to_owned(),
                Some("service=git-receive-pack".to_owned()),
                basic.clone(),
                String::new(),
                vec![],
            ),
            (
                "pack application/x-git-receive-pack-result".to_owned(),
                None,
                basic,
                "application/x-git-receive-pack-request".to_owned(),
                pack,
            ),
        ]
    );

    // Failures carry no upstream body, and anything that is not git's own
    // answer is refused.
    let refused = gateway
        .call(call(&bound, format!("{origin}/denied"), None))
        .await
        .unwrap();
    assert_eq!(reply(refused), (403, vec![]));
    assert_eq!(
        denied(
            gateway
                .call(call(&bound, format!("{origin}/login"), None))
                .await
        ),
        BrokerError::Upstream(UpstreamFailure::NotGit)
    );
    // The token goes only to the origin it is bound to.
    assert_eq!(
        denied(
            gateway
                .call(call(&elsewhere, repository.clone(), None))
                .await
        ),
        BrokerError::Denied(a206_vault::Denial::DestinationNotBound)
    );
    let host = origin.trim_start_matches("http://");
    for bad in [
        format!("{repository}?service=upload-pack"),
        format!("{repository}#refs"),
        format!("http://user@{host}/team/repo.git"),
        format!("http://{}.invalid/team/repo.git", random()),
        random(),
    ] {
        assert_eq!(
            denied(gateway.call(call(&bound, bad.clone(), None)).await),
            BrokerError::Invalid(InvalidRequest::GitRepository),
            "{bad}"
        );
    }
    for bad in [
        String::new(),
        format!("{}:{}", random(), random()),
        "a\nb".into(),
    ] {
        let request = Request::GitReceivePack(GitCall {
            credential: bound.clone(),
            username: bad,
            repository: repository.clone(),
            request: None,
        });
        assert_eq!(
            denied(gateway.call(request).await),
            BrokerError::Invalid(InvalidRequest::GitUsername)
        );
    }

    // A fetch goes to the upload-pack service, with the same credential
    // handling, and only accepts that service's answers.
    seen.lock().unwrap().clear();
    let fetch = |repository: String, request: Option<Vec<u8>>| {
        Request::GitUploadPack(GitCall {
            credential: bound.clone(),
            username: username.clone(),
            repository,
            request,
        })
    };
    let advertisement = gateway.call(fetch(repository.clone(), None)).await.unwrap();
    assert_eq!(reply(advertisement), (200, advertised));
    let wants = random().into_bytes();
    let result = gateway
        .call(fetch(repository.clone(), Some(wants.clone())))
        .await
        .unwrap();
    assert_eq!(reply(result), (200, answered));
    let basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{token}"))
    );
    assert_eq!(
        *seen.lock().unwrap(),
        [
            (
                "refs application/x-git-upload-pack-advertisement".to_owned(),
                Some("service=git-upload-pack".to_owned()),
                basic.clone(),
                String::new(),
                vec![],
            ),
            (
                "pack application/x-git-upload-pack-result".to_owned(),
                None,
                basic,
                "application/x-git-upload-pack-request".to_owned(),
                wants,
            ),
        ]
    );
    assert_eq!(
        denied(gateway.call(fetch(format!("{origin}/mixed"), None)).await),
        BrokerError::Upstream(UpstreamFailure::NotGit)
    );
    assert_eq!(
        denied(a.call(fetch(repository.clone(), None)).await),
        BrokerError::Forbidden
    );
}

/// What an SSH client signs to authenticate with `key`: a session
/// identifier and a public-key user authentication request (RFC 4252).
fn userauth(key: &ssh_key::PublicKey) -> Vec<u8> {
    fn put(out: &mut Vec<u8>, bytes: &[u8]) {
        out.extend(u32::try_from(bytes.len()).unwrap().to_be_bytes());
        out.extend(bytes);
    }
    let mut data = Vec::new();
    put(&mut data, uuid::Uuid::new_v4().as_bytes());
    data.push(50);
    put(&mut data, random().as_bytes());
    put(&mut data, b"ssh-connection");
    put(&mut data, b"publickey");
    data.push(1);
    put(&mut data, key.algorithm().as_str().as_bytes());
    put(&mut data, &key.to_bytes().unwrap());
    data
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bound_ssh_key_signs_authentication_for_a_paired_gateway_only() {
    let (_dir, root, _task) = start().await;
    let mut a = user(&root).await;
    a.call(unlock(&random(), None, true)).await.unwrap();
    let (name, token) = (random(), random());
    let host = format!("{}.test", uuid::Uuid::new_v4().simple());
    let (private, public) = a206_vault::generate_ssh_key(&random()).unwrap();
    let mut held = meta(vec![format!("SSH://{}:2222/", host.to_uppercase())]);
    held.kind = CredentialKind::SshKey;
    a.call(Request::PutCredential {
        name: name.clone(),
        meta: held,
        value: WireValue::SshKeyPair {
            private_key: WireSecret::new(private.to_string()),
            public_key: WireSecret::new(public.clone()),
        },
        replace: false,
    })
    .await
    .unwrap();
    // The destination is stored in its exact form.
    let Reply::Credentials { typed, .. } = a.call(Request::Credentials).await.unwrap() else {
        panic!("unexpected reply");
    };
    let destination = format!("ssh://{host}:2222");
    assert_eq!(
        typed
            .iter()
            .find(|(n, _)| *n == name)
            .unwrap()
            .1
            .destinations,
        std::slice::from_ref(&destination)
    );
    let key = ssh_key::PublicKey::from_openssh(&public).unwrap();
    let call = |credential: &str, destination: &str, sign: Option<Vec<u8>>| {
        Request::SshAgent(SshAgentCall {
            credential: credential.to_owned(),
            destination: destination.to_owned(),
            sign,
        })
    };
    let data = userauth(&key);
    assert_eq!(
        denied(a.call(call(&name, &destination, Some(data.clone()))).await),
        BrokerError::Forbidden
    );
    let mut gateway = paired_gateway(&mut a, &root).await;
    let Reply::SshAgent {
        public_key,
        signature,
    } = gateway.call(call(&name, &destination, None)).await.unwrap()
    else {
        panic!("unexpected reply");
    };
    assert_eq!(public_key, key.to_bytes().unwrap());
    assert_eq!(signature, None);
    let Reply::SshAgent {
        signature: Some(signature),
        ..
    } = gateway
        .call(call(&name, &destination, Some(data.clone())))
        .await
        .unwrap()
    else {
        panic!("unexpected reply");
    };
    let signature =
        <ssh_key::Signature as ssh_encoding::Decode>::decode(&mut signature.as_slice()).unwrap();
    signature::Verifier::verify(&key, &data, &signature).unwrap();
    // Nothing but an authentication request for the key is signed.
    assert!(matches!(
        denied(
            gateway
                .call(call(&name, &destination, Some(token.into_bytes())))
                .await
        ),
        BrokerError::SshKey(a206_vault::SshKeyError::NotUserAuth(_))
    ));
    // Only the bound server, written exactly.
    for (other, expected) in [
        (
            format!("ssh://{host}:22"),
            BrokerError::Denied(a206_vault::Denial::DestinationNotBound),
        ),
        (
            format!("ssh://{host}:2222/"),
            BrokerError::Invalid(InvalidRequest::SshAgent),
        ),
        (
            format!("ssh://git@{host}:2222"),
            BrokerError::Invalid(InvalidRequest::SshAgent),
        ),
    ] {
        assert_eq!(
            denied(gateway.call(call(&name, &other, Some(data.clone()))).await),
            expected,
            "{other}"
        );
    }
    assert_eq!(
        denied(
            gateway
                .call(call(
                    &name,
                    &destination,
                    Some(vec![0; MAX_SSH_SIGN_DATA + 1])
                ))
                .await
        ),
        BrokerError::Invalid(InvalidRequest::SshAgent)
    );
    assert_eq!(
        denied(gateway.call(call(&random(), &destination, None)).await),
        BrokerError::NotFound
    );
    // Locking ends the gateway's use of the key.
    a.call(Request::Lock).await.unwrap();
    assert!(matches!(
        denied(gateway.call(call(&name, &destination, None)).await),
        BrokerError::Locked | BrokerError::GatewayNotPaired
    ));
}
fn key_input(name: &str, value: &str) -> Request {
    Request::AddApiKey(ApiKeyInput {
        version: a206_vault_channel::CREDENTIALS_SCHEMA_VERSION,
        name: name.into(),
        label: random(),
        destinations: vec!["https://example.invalid".into()],
        value: WireSecret::new(value.into()),
    })
}
fn page_request(after: Option<String>, limit: u16) -> Request {
    Request::CredentialPage {
        version: a206_vault_channel::CREDENTIALS_SCHEMA_VERSION,
        after,
        limit,
    }
}
#[tokio::test(flavor = "multi_thread")]
async fn credential_pages_and_additions_require_user_auth_and_never_replace_or_reveal() {
    let (_dir, root, task) = start().await;
    let mut a = user(&root).await;
    assert!(matches!(
        denied(a.call(page_request(None, 5)).await),
        BrokerError::Locked
    ));
    let password = random();
    a.call(unlock(&password, None, true)).await.unwrap();
    let mut other = user(&root).await;
    let name = random();
    let value = random();
    assert_eq!(
        denied(other.call(key_input(&name, &value)).await),
        BrokerError::StepUpRequired
    );
    let mut gateway = paired_gateway(&mut a, &root).await;
    assert_eq!(
        denied(gateway.call(page_request(None, 5)).await),
        BrokerError::Forbidden
    );
    assert_eq!(
        denied(gateway.call(key_input(&name, &value)).await),
        BrokerError::Forbidden
    );
    a.call(key_input(&name, &value)).await.unwrap();
    assert_eq!(
        denied(a.call(key_input(&name, &random())).await),
        BrokerError::AlreadyExists
    );
    let Reply::CredentialPage(page) = a.call(page_request(None, 1)).await.unwrap() else {
        panic!()
    };
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].0, name);
    assert_eq!(page.entries[0].1.policy, AccessPolicy::WithApproval);
    assert_eq!(page.entries[0].1.kind, CredentialKind::ApiKey);
    assert_eq!(page.entries[0].1.destinations, ["https://example.invalid"]);
    assert!(!format!("{page:?}").contains(&value));
    assert!(!status(&mut a).await.agent_access.unwrap());
    let second = random();
    a.call(key_input(&second, &random())).await.unwrap();
    let Reply::CredentialPage(first) = a.call(page_request(None, 1)).await.unwrap() else {
        panic!()
    };
    let Reply::CredentialPage(last) = a.call(page_request(first.next.clone(), 1)).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(first.total, 2);
    assert!(first.next.is_some());
    assert!(last.next.is_none());
    assert!(first.entries[0].0 < last.entries[0].0);
    for limit in [0, a206_vault_channel::MAX_CREDENTIAL_PAGE + 1] {
        assert_eq!(
            denied(a.call(page_request(None, limit)).await),
            BrokerError::Invalid(InvalidRequest::CredentialPage)
        );
    }
    for dest in [
        "https://example.invalid/path",
        "https://example.invalid/?q=x",
        "https://example.invalid/#x",
        "http://example.invalid",
        "https://user:pass@example.invalid",
        "ssh://example.invalid/",
        "SSH://example.invalid/",
        "ircs://example.invalid/",
    ] {
        let Request::AddApiKey(mut input) = key_input(&random(), &random()) else {
            panic!()
        };
        input.destinations = vec![dest.into()];
        assert_eq!(
            denied(a.call(Request::AddApiKey(input)).await),
            BrokerError::Invalid(InvalidRequest::Destination)
        );
    }
    for value in [
        String::new(),
        "x".repeat(a206_vault_channel::MAX_API_KEY_BYTES + 1),
    ] {
        assert_eq!(
            denied(a.call(key_input(&random(), &value)).await),
            BrokerError::Invalid(InvalidRequest::ApiKeyValue)
        );
    }
    a.call(Request::Lock).await.unwrap();
    assert!(matches!(
        denied(a.call(page_request(None, 1)).await),
        BrokerError::Locked
    ));
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
}

#[tokio::test(flavor = "multi_thread")]
async fn encrypted_credentials_share_the_same_policy_and_preserve_values_in_the_broker() {
    use a206_vault_channel::{CREDENTIALS_SCHEMA_VERSION as V, Reply as R, Request as Q, Secret};
    let (_dir, root, task) = start().await;
    let (mut relay, mut channel) = secure_open(&root).await;
    assert!(matches!(
        secure_call(
            &mut relay,
            &mut channel,
            Q::Unlock {
                password: Secret::new(random()),
                totp: None,
                create: true
            }
        )
        .await,
        R::Ok
    ));
    let name = random();
    let value = random();
    let add = || Q::AddApiKey {
        version: V,
        name: name.clone(),
        label: random(),
        destinations: vec![],
        value: Secret::new(value.clone()),
    };
    assert!(matches!(
        secure_call(&mut relay, &mut channel, add()).await,
        R::Ok
    ));
    assert!(
        matches!(secure_call(&mut relay, &mut channel, add()).await, R::Error(e) if e.report.kind == a206_core::ErrorKind::Conflict)
    );
    let R::CredentialPage(page) = secure_call(
        &mut relay,
        &mut channel,
        Q::CredentialPage {
            version: V,
            after: None,
            limit: 5,
        },
    )
    .await
    else {
        panic!()
    };
    assert_eq!(page.entries[0].0, name);
    assert!(page.entries[0].1.destinations.is_empty());
    let encoded = a206_protocol::encode(&page).unwrap();
    assert!(!encoded.windows(value.len()).any(|w| w == value.as_bytes()));
    assert!(matches!(
        secure_call(
            &mut relay,
            &mut channel,
            Q::CredentialPage {
                version: V + 1,
                after: None,
                limit: 5
            }
        )
        .await,
        R::Error(_)
    ));
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
}

#[tokio::test(flavor = "multi_thread")]
async fn metadata_pages_bound_bytes_without_truncating_policies() {
    let (_dir, root, task) = start().await;
    let mut a = user(&root).await;
    a.call(unlock(&random(), None, true)).await.unwrap();
    for prefix in ["a", "b", "c"] {
        let mut meta = meta(vec![]);
        meta.policy = AccessPolicy::SkillOnly(vec![random().repeat(500)]);
        a.call(Request::PutCredential {
            name: format!("{prefix}-{}", random()),
            meta,
            value: WireValue::Single(WireSecret::new(random())),
            replace: false,
        })
        .await
        .unwrap();
    }
    let Reply::CredentialPage(page) = a.call(page_request(None, 50)).await.unwrap() else {
        panic!()
    };
    assert_eq!(page.entries.len(), 2);
    assert!(page.next.is_some());
    assert!(
        a206_protocol::encode(&page).unwrap().len()
            <= a206_vault_channel::MAX_CREDENTIAL_PAGE_BYTES
    );
    assert!(
        matches!(&page.entries[0].1.policy, AccessPolicy::SkillOnly(scopes) if scopes[0].len() == 36 * 500)
    );
    let mut big = meta(vec![]);
    big.policy = AccessPolicy::TriggerOnly(vec![random().repeat(2000)]);
    a.call(Request::PutCredential {
        name: format!("z-{}", random()),
        meta: big,
        value: WireValue::Single(WireSecret::new(random())),
        replace: false,
    })
    .await
    .unwrap();
    assert_eq!(
        denied(a.call(page_request(Some("y".into()), 50)).await),
        BrokerError::Vault(VaultFailure::MetadataTooLarge)
    );
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn credential_management_refuses_old_peers_before_sending() {
    let broker = scripted_broker(CREDENTIALS_VERSION - 1, vec![Reply::Ok]).await;
    let mut a = user(broker.path()).await;
    for request in [page_request(None, 5), key_input(&random(), &random())] {
        assert!(matches!(
            a.call(request).await,
            Err(ClientError::CredentialsUnsupported)
        ));
    }
    assert!(matches!(a.call(Request::Status).await.unwrap(), Reply::Ok));
}
