#![cfg(unix)]
//! Shared vault operations against a real broker (and gateway) in a temporary
//! profile. Brokers run in-process: the test binary is not `agent206`, so
//! nothing here may fall back to spawning one.
use a206_broker::{BrokerError, ClientError, Reply, Request, WireSecret};
use a206_core::ErrorKind;
use a206_runtime::vault::{self, Session, VaultError};
use std::{path::Path, time::Duration};

async fn wait_for(path: &Path) {
    for _ in 0..500 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{} never appeared", path.display());
}

async fn start_broker() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_owned();
    tokio::spawn(async move { a206_broker::serve(&root).await.unwrap() });
    wait_for(&a206_broker::socket_path(dir.path())).await;
    dir
}

fn create(password: &str) -> Request {
    Request::Unlock {
        password: WireSecret::new(password.to_owned()),
        totp: None,
        create: true,
    }
}

#[tokio::test]
async fn sessions_report_state_and_keep_step_up_per_connection() {
    let dir = start_broker().await;
    let mut direct = vault::connect(dir.path()).await.unwrap();
    let before = vault::status(&mut direct).await.unwrap();
    assert!(!before.exists && !before.unlocked);
    assert_eq!(before.totp_enrolled, None);

    let session = Session::open(dir.path().to_owned());
    let password = uuid::Uuid::new_v4().to_string();
    session.call(create(&password)).await.unwrap();
    let after = session.status().await.unwrap();
    assert!(after.exists && after.unlocked);
    assert_eq!(after.totp_enrolled, Some(false));
    // The unlock's step-up belongs to the session's own connection.
    assert!(after.step_up_active);
    assert!(!vault::status(&mut direct).await.unwrap().step_up_active);
    let other = Session::open(dir.path().to_owned());
    assert!(!other.status().await.unwrap().step_up_active);

    // A broker refusal reaches the caller typed, and the session stays usable.
    let wrong = other
        .call(Request::StepUp {
            password: Some(WireSecret::new(uuid::Uuid::new_v4().to_string())),
            totp: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(
        wrong,
        VaultError::Broker(ClientError::Broker(BrokerError::AuthenticationFailed))
    ));
    assert_eq!(
        a206_runtime::error::report(&wrong).kind,
        ErrorKind::PermissionDenied
    );
    other
        .call(Request::StepUp {
            password: Some(WireSecret::new(password)),
            totp: None,
        })
        .await
        .unwrap();
    assert!(other.status().await.unwrap().step_up_active);

    session.call(Request::Lock).await.unwrap();
    let locked = other.status().await.unwrap();
    assert!(locked.exists && !locked.unlocked && !locked.step_up_active);
}

#[tokio::test]
async fn pairing_reaches_the_running_gateway_only_with_step_up() {
    let dir = start_broker().await;
    let root = dir.path().to_owned();
    let session = Session::open(root.clone());
    session
        .call(create(&uuid::Uuid::new_v4().to_string()))
        .await
        .unwrap();

    // No gateway yet: nothing is paired, and a strict pairing says why.
    assert!(!vault::gateway_running(&root).await);
    assert!(!session.pair_if_running().await.unwrap());
    let offline = session.pair_gateway().await.unwrap_err();
    assert!(matches!(offline, VaultError::Gateway(_)), "{offline:?}");
    assert_eq!(
        a206_runtime::error::report(&offline).kind,
        ErrorKind::Unavailable
    );

    let serving = root.clone();
    let gateway = tokio::spawn(async move { a206_runtime::service::daemon(&serving, true).await });
    wait_for(&root.join("gateway.sock")).await;
    assert!(vault::gateway_running(&root).await);
    assert!(session.pair_if_running().await.unwrap());

    // A connection without step-up cannot mint a pairing.
    let fresh = Session::open(root.clone());
    let refused = fresh.pair_gateway().await.unwrap_err();
    assert!(
        matches!(
            refused,
            VaultError::Broker(ClientError::Broker(BrokerError::StepUpRequired))
        ),
        "{refused:?}"
    );
    gateway.abort();
}

/// A broker from another protocol version is named as such, with the
/// recovery, instead of being replaced by a second broker.
#[tokio::test]
async fn an_incompatible_broker_is_reported_with_its_recovery() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("vault")).unwrap();
    let listener = tokio::net::UnixListener::bind(a206_broker::socket_path(dir.path())).unwrap();
    let old = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut decoder = a206_protocol::Decoder::default();
        let mut bytes = vec![0; 8192];
        loop {
            let count = stream.read(&mut bytes).await.unwrap();
            let requests: Vec<a206_protocol::Request<Request>> =
                decoder.feed(&bytes[..count]).unwrap();
            if let Some(hello) = requests.into_iter().next() {
                let reply = a206_protocol::Response {
                    id: hello.id,
                    body: Reply::Error(BrokerError::Invalid(
                        a206_broker::InvalidRequest::UnsupportedVersion,
                    )),
                };
                stream
                    .write_all(&a206_protocol::encode(&reply).unwrap())
                    .await
                    .unwrap();
                return;
            }
        }
    });
    let error = match vault::connect(dir.path()).await {
        Ok(_) => panic!("connected to an incompatible broker"),
        Err(error) => error,
    };
    assert!(matches!(error, VaultError::VersionMismatch(_)), "{error:?}");
    let report = a206_runtime::error::report(&error);
    assert_eq!(report.kind, ErrorKind::Unavailable);
    assert!(report.message.contains("agent206 vault stop"), "{report:?}");
    assert!(report.causes().next().is_some(), "{report:?}");
    old.await.unwrap();
}
