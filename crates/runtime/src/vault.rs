//! Trusted-client operations on the vault broker, shared by the CLI and the
//! desktop and terminal clients so every surface starts, queries and pairs it
//! the same way. The broker's own requests, throttling and step-up rules are
//! the policy; nothing here relaxes them.
use a206_core::ErrorKind;
use std::path::PathBuf;

/// Why a vault operation failed. Broker refusals keep their typed
/// [`a206_broker::BrokerError`]; the gateway's refusal of a pairing keeps its
/// report as the cause.
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error(
        "the running vault broker is from a different Agent 206 version; \
         run `agent206 vault stop`, then retry"
    )]
    VersionMismatch(#[source] a206_broker::ClientError),
    #[error("could not prepare the vault directory")]
    Directory(#[source] std::io::Error),
    #[error("could not open the vault broker log")]
    Log(#[source] std::io::Error),
    #[error("could not locate the agent206 executable")]
    CurrentExe(#[source] std::io::Error),
    #[error("could not start the vault broker")]
    Spawn(#[source] std::io::Error),
    #[error("vault broker did not start; inspect {}", log.display())]
    NotReady {
        log: PathBuf,
        #[source]
        source: Option<a206_broker::ClientError>,
    },
    #[error(transparent)]
    Broker(#[from] a206_broker::ClientError),
    #[error("unexpected vault broker reply")]
    UnexpectedReply,
    #[error(transparent)]
    Gateway(crate::service::ServiceError),
    #[error("could not exchange the pairing with the gateway")]
    GatewayChannel(#[source] crate::transport::FrameError),
    #[error("the gateway did not answer the pairing in time")]
    GatewayTimeout(#[source] tokio::time::error::Elapsed),
    #[error("the gateway closed the connection before answering the pairing")]
    GatewayClosed,
    #[error("the gateway refused the pairing")]
    PairingRefused(#[source] a206_core::ErrorReport),
    #[error("unexpected gateway reply")]
    UnexpectedGatewayReply,
    #[error("the vault connection worker stopped")]
    Worker(#[source] Option<std::sync::Arc<std::io::Error>>),
    #[error("vault controls are not available on this platform")]
    Unsupported,
}
impl crate::error::Classified for VaultError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::VersionMismatch(_) | Self::NotReady { .. } | Self::GatewayClosed => {
                ErrorKind::Unavailable
            }
            Self::UnexpectedReply | Self::UnexpectedGatewayReply => ErrorKind::Protocol,
            Self::GatewayTimeout(_) => ErrorKind::Timeout,
            Self::CurrentExe(_) | Self::Worker(_) => ErrorKind::Internal,
            Self::Unsupported => ErrorKind::Unavailable,
            // `transparent` hides these from the chain, so classify them here.
            Self::Broker(error) => return crate::error::classify(error),
            Self::Gateway(error) => return crate::error::classify(error),
            // The cause (I/O, frame or the gateway's own report) knows the kind.
            Self::Directory(_)
            | Self::Log(_)
            | Self::Spawn(_)
            | Self::GatewayChannel(_)
            | Self::PairingRefused(_) => return None,
        })
    }
}
type Result<T, E = VaultError> = std::result::Result<T, E>;

#[cfg(unix)]
pub use imp::*;
#[cfg(unix)]
mod imp {
    use super::{Result, VaultError};
    use a206_broker::{
        BrokerError, BrokerStatus, Client, ClientError, InvalidRequest, Reply, Request, Role,
    };
    use a206_core::Ignore;
    use std::path::{Path, PathBuf};
    use tokio::sync::{mpsc, oneshot};

    /// Connect to this profile's broker only if one is running; `None` when
    /// none answers. Never starts a broker.
    pub async fn connect_running(root: &Path) -> std::result::Result<Option<Client>, ClientError> {
        match Client::connect(root, Role::User).await {
            Ok(client) => Ok(Some(client)),
            Err(ClientError::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// Connect to this profile's broker, starting `agent206 vault broker` when
    /// none is running. The broker outlives the caller, like the gateway.
    pub async fn connect(root: &Path) -> Result<Client> {
        match Client::connect(root, Role::User).await {
            Ok(client) => return Ok(client),
            Err(
                error @ ClientError::Broker(BrokerError::Invalid(
                    InvalidRequest::UnsupportedVersion,
                )),
            ) => return Err(VaultError::VersionMismatch(error)),
            // Not running yet: start it below. Startup failure is reported instead.
            Err(_) => {}
        }
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let dir = root.join("vault");
        std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)))
            .map_err(VaultError::Directory)?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(dir.join("broker.log"))
            .map_err(VaultError::Log)?;
        let mut child =
            std::process::Command::new(std::env::current_exe().map_err(VaultError::CurrentExe)?)
                .arg("--data-dir")
                .arg(root)
                .args(["vault", "broker"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(log)
                .spawn()
                .map_err(VaultError::Spawn)?;
        let mut last = None;
        for _ in 0..100 {
            match Client::connect(root, Role::User).await {
                Ok(client) => {
                    // Deliberately detached: the broker outlives this caller,
                    // and this thread only reaps it if it exits first.
                    std::thread::spawn(move || child.wait());
                    return Ok(client);
                }
                Err(error) => last = Some(error),
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        // Killing fails only if the broker already exited, which is the goal.
        child.kill().ignore();
        Err(VaultError::NotReady {
            log: dir.join("broker.log"),
            source: last,
        })
    }

    pub async fn status(client: &mut Client) -> Result<BrokerStatus> {
        match client.call(Request::Status).await? {
            Reply::Status(status) => Ok(status),
            _ => Err(VaultError::UnexpectedReply),
        }
    }

    /// Whether this profile's local gateway is accepting connections.
    pub async fn gateway_running(root: &Path) -> bool {
        crate::service::connect(root, false, false).await.is_ok()
    }

    /// Mint this unlock's gateway pairing (the connection must already hold a
    /// step-up) and hand it straight to the running local gateway, never to
    /// disk. The gateway is reached first, so a stopped gateway does not void
    /// an earlier pairing.
    pub async fn pair_gateway(client: &mut Client, root: &Path) -> Result<()> {
        use a206_core::{BrokerPairing, Command, Reply as GatewayReply};
        let stream = crate::service::connect(root, false, false)
            .await
            .map_err(VaultError::Gateway)?;
        let Reply::GatewayPairing { token } = client.call(Request::PairGateway).await? else {
            return Err(VaultError::UnexpectedReply);
        };
        let pairing = BrokerPairing::new(token.expose().to_owned());
        drop(token);
        let (mut read, mut write) = stream.into_split();
        let id = uuid::Uuid::new_v4();
        let exchange = async {
            crate::transport::write_frame(
                &mut write,
                &a206_protocol::Request {
                    id,
                    body: Command::PairBroker { pairing },
                },
            )
            .await?;
            loop {
                match crate::transport::read_frame::<a206_protocol::Response<GatewayReply>>(
                    &mut read,
                )
                .await?
                {
                    Some(response) if response.id == id => return Ok(Some(response.body)),
                    // Unrelated frames (none are expected) are skipped.
                    Some(_) => {}
                    None => return Ok(None),
                }
            }
        };
        let reply = tokio::time::timeout(std::time::Duration::from_secs(30), exchange)
            .await
            .map_err(VaultError::GatewayTimeout)?
            .map_err(VaultError::GatewayChannel)?;
        match reply {
            Some(GatewayReply::Ok) => Ok(()),
            Some(GatewayReply::Error(report)) => Err(VaultError::PairingRefused(report)),
            Some(_) => Err(VaultError::UnexpectedGatewayReply),
            None => Err(VaultError::GatewayClosed),
        }
    }

    enum Job {
        /// Read the status without starting a broker; `None` when none runs.
        Peek(oneshot::Sender<Result<Option<BrokerStatus>>>),
        /// Work that starts a broker when none is running.
        Act(Action),
    }
    enum Action {
        Call(Box<Request>, oneshot::Sender<Result<Reply>>),
        Pair {
            if_running: bool,
            reply: oneshot::Sender<Result<bool>>,
        },
    }

    /// One broker connection owned by a dedicated worker, for clients whose
    /// executor is not Tokio (the desktop and terminal UI). Step-up and a
    /// pending two-factor enrollment belong to the connection, so a session
    /// keeps them across calls. A lost connection is reported once and
    /// reopened, without step-up, for the next call.
    #[derive(Clone)]
    pub struct Session {
        sender: mpsc::UnboundedSender<Job>,
    }
    impl Session {
        pub fn open(root: PathBuf) -> Self {
            let (sender, receiver) = mpsc::unbounded_channel();
            // Deliberately detached: the worker ends when the last handle is
            // dropped, and each caller learns of its loss through its reply.
            std::thread::spawn(move || {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime.block_on(work(root, receiver)),
                    Err(error) => fail_all(receiver, error),
                }
            });
            Self { sender }
        }
        pub async fn call(&self, request: Request) -> Result<Reply> {
            let (reply, receiver) = oneshot::channel();
            self.sender
                .send(Job::Act(Action::Call(Box::new(request), reply)))
                .map_err(|_| VaultError::Worker(None))?;
            receiver.await.map_err(|_| VaultError::Worker(None))?
        }
        pub async fn status(&self) -> Result<BrokerStatus> {
            match self.call(Request::Status).await? {
                Reply::Status(status) => Ok(status),
                _ => Err(VaultError::UnexpectedReply),
            }
        }
        /// The broker's status if one is running, without starting one, for
        /// background reads that must not undo a deliberate `vault stop`.
        pub async fn status_if_running(&self) -> Result<Option<BrokerStatus>> {
            let (reply, receiver) = oneshot::channel();
            self.sender
                .send(Job::Peek(reply))
                .map_err(|_| VaultError::Worker(None))?;
            receiver.await.map_err(|_| VaultError::Worker(None))?
        }
        /// Pair the running gateway with this connection's unlock.
        pub async fn pair_gateway(&self) -> Result<()> {
            self.pair(false).await.map(drop)
        }
        /// Pair only if the gateway is running; `Ok(false)` when it is not.
        pub async fn pair_if_running(&self) -> Result<bool> {
            self.pair(true).await
        }
        async fn pair(&self, if_running: bool) -> Result<bool> {
            let (reply, receiver) = oneshot::channel();
            self.sender
                .send(Job::Act(Action::Pair { if_running, reply }))
                .map_err(|_| VaultError::Worker(None))?;
            receiver.await.map_err(|_| VaultError::Worker(None))?
        }
    }

    fn fail_all(mut receiver: mpsc::UnboundedReceiver<Job>, error: std::io::Error) {
        let error = std::sync::Arc::new(error);
        let lost = || VaultError::Worker(Some(error.clone()));
        while let Some(job) = receiver.blocking_recv() {
            // A caller that stopped waiting needs no reply.
            match job {
                Job::Act(Action::Call(_, reply)) => reply.send(Err(lost())).ignore(),
                Job::Act(Action::Pair { reply, .. }) => reply.send(Err(lost())).ignore(),
                Job::Peek(reply) => reply.send(Err(lost())).ignore(),
            }
        }
    }

    /// Status through the held connection, or a running broker's; never
    /// starts one. A held connection that fails in transport may belong to a
    /// broker that was stopped, so look again before reporting it.
    async fn peek(root: &Path, client: &mut Option<Client>) -> Result<Option<BrokerStatus>> {
        if let Some(mut connected) = client.take() {
            let result = status(&mut connected).await;
            if keeps_connection(&result) {
                *client = Some(connected);
                return result.map(Some);
            }
        }
        let Some(mut connected) = connect_running(root).await? else {
            return Ok(None);
        };
        let result = status(&mut connected).await;
        if keeps_connection(&result) {
            *client = Some(connected);
        }
        result.map(Some)
    }

    async fn work(root: PathBuf, mut receiver: mpsc::UnboundedReceiver<Job>) {
        let mut client = None;
        while let Some(job) = receiver.recv().await {
            let job = match job {
                Job::Peek(reply) => {
                    // A caller that stopped waiting needs no reply.
                    reply.send(peek(&root, &mut client).await).ignore();
                    continue;
                }
                Job::Act(action) => action,
            };
            let connected = match client.take() {
                Some(connected) => Ok(connected),
                None => connect(&root).await,
            };
            let mut connected = match connected {
                Ok(connected) => connected,
                Err(error) => {
                    // A caller that stopped waiting needs no reply.
                    match job {
                        Action::Call(_, reply) => reply.send(Err(error)).ignore(),
                        Action::Pair { reply, .. } => reply.send(Err(error)).ignore(),
                    }
                    continue;
                }
            };
            let healthy = match job {
                Action::Call(request, reply) => {
                    let result = connected.call(*request).await.map_err(VaultError::from);
                    let healthy = keeps_connection(&result);
                    reply.send(result).ignore();
                    healthy
                }
                Action::Pair { if_running, reply } => {
                    let result = if if_running && !gateway_running(&root).await {
                        Ok(false)
                    } else {
                        pair_gateway(&mut connected, &root).await.map(|()| true)
                    };
                    let healthy = keeps_connection(&result);
                    reply.send(result).ignore();
                    healthy
                }
            };
            if healthy {
                client = Some(connected);
            }
        }
    }

    /// A broker refusal leaves the connection usable; a transport failure does not.
    fn keeps_connection<T>(result: &Result<T>) -> bool {
        !matches!(
            result,
            Err(VaultError::Broker(
                ClientError::Io(_) | ClientError::Closed | ClientError::Protocol(_)
            ))
        )
    }
}

#[cfg(not(unix))]
pub use fallback::*;
#[cfg(not(unix))]
mod fallback {
    use super::{Result, VaultError};
    use std::path::{Path, PathBuf};

    /// The broker uses Unix sockets; other platforms have no vault controls yet.
    #[derive(Clone)]
    pub struct Session;
    impl Session {
        pub fn open(_root: PathBuf) -> Self {
            Self
        }
        pub async fn call(&self, _request: a206_broker::Request) -> Result<a206_broker::Reply> {
            Err(VaultError::Unsupported)
        }
        pub async fn status(&self) -> Result<a206_broker::BrokerStatus> {
            Err(VaultError::Unsupported)
        }
        pub async fn status_if_running(&self) -> Result<Option<a206_broker::BrokerStatus>> {
            Err(VaultError::Unsupported)
        }
        pub async fn pair_gateway(&self) -> Result<()> {
            Err(VaultError::Unsupported)
        }
        pub async fn pair_if_running(&self) -> Result<bool> {
            Err(VaultError::Unsupported)
        }
    }
    pub async fn gateway_running(_root: &Path) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a206_core::ErrorReport;

    #[test]
    fn a_refused_pairing_keeps_the_gateway_report() {
        let message = uuid::Uuid::new_v4().to_string();
        let cause = uuid::Uuid::new_v4().to_string();
        let refusal = ErrorReport::new(ErrorKind::PermissionDenied, message.clone())
            .caused_by(ErrorReport::new(ErrorKind::Other, cause.clone()));
        let report = crate::error::report(&VaultError::PairingRefused(refusal));
        assert_eq!(report.kind, ErrorKind::PermissionDenied);
        let causes: Vec<_> = report.causes().map(|c| c.message.as_str()).collect();
        assert_eq!(causes, [message.as_str(), cause.as_str()]);
    }

    #[test]
    fn broker_refusals_keep_their_kind_through_the_wrapper() {
        let locked = VaultError::from(a206_broker::ClientError::from(
            a206_broker::BrokerError::Locked,
        ));
        let report = crate::error::report(&locked);
        assert_eq!(report.kind, ErrorKind::Unavailable);
        assert_eq!(report.message, a206_broker::BrokerError::Locked.to_string());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_background_status_read_never_starts_a_broker() {
        let root = tempfile::tempdir().unwrap();
        let session = Session::open(root.path().to_path_buf());
        assert!(matches!(session.status_if_running().await, Ok(None)));
        // Nothing was spawned or prepared for one.
        assert!(!a206_broker::socket_path(root.path()).exists());
        assert!(!root.path().join("vault").exists());
    }
}
