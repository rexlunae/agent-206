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
    let token = pairing(&mut a).await;
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
    a.call(unlock(&password, None, false)).await.unwrap();
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

/// A broker double that greets like `version` and answers a stream request
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
                    Request::ProviderStream(_) => frames.lock().unwrap().take().unwrap(),
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
