//! Messengers: chat services that reach threads through the gateway.
//!
//! Each enabled messenger runs as one supervised task. It spends its bot
//! token only through the vault broker, for the service's API origin, so the
//! gateway never holds the value. A message from a chat the user has not
//! allowed is refused and listed in the messenger's status; an allowed chat
//! is bound to one thread, and each message becomes one ordinary turn there
//! through [`Host::converse`], under that thread's mode, grants and approvals.
//! A person in an allowed chat can therefore do whatever the user could do
//! by typing into that thread, and nothing more.
mod telegram;

use a206_broker::{AuthScheme, HttpMethod, ProviderCall, Reply as BrokerReply, Request, Role};
use a206_core::{BrokerPairing, ChatBinding, ErrorKind, Id, MessengerConfig, MessengerKind};
use a206_core::{MessengerState, MessengerStatus};
use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// Why a messenger could not connect, receive or deliver.
#[derive(Debug, thiserror::Error)]
pub enum MessengerError {
    #[error("the messenger has no vault credential; add its bot token and bind it to {origin}")]
    NoCredential { origin: String },
    #[error("the messenger endpoint is not allowed")]
    Endpoint(#[source] a206_core::providers::EndpointError),
    #[error("a messenger endpoint is an origin, without a path")]
    EndpointPath,
    #[error(
        "the running vault broker predates messengers; stop it with `agent206 vault stop` and unlock again"
    )]
    BrokerTooOld,
    #[error("could not reach the vault broker")]
    VaultUnavailable(#[source] a206_broker::ClientError),
    #[error("the vault broker refused the request")]
    Broker(#[source] a206_broker::ClientError),
    #[error("the vault broker sent an unexpected reply")]
    UnexpectedReply,
    #[error("{service} rejected the bot token; store a valid one in the vault")]
    Unauthorized { service: &'static str },
    #[error(
        "{service} refused polling because another client or a webhook uses this bot; stop the other client or delete the webhook"
    )]
    PollingConflict { service: &'static str },
    #[error("{service} answered with HTTP status {status}")]
    Status { service: &'static str, status: u16 },
    #[error("{service} sent a response that could not be read")]
    Malformed {
        service: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("{service} reported a failed request")]
    Refused { service: &'static str },
    #[error("choose a model for this messenger before allowing chats")]
    NoModel,
    #[error("the thread could not answer the message")]
    Conversation(#[source] Box<crate::gateway::GatewayError>),
    #[error("the thread answered with an error")]
    Turn(#[source] a206_core::ErrorReport),
    #[error("the turn stopped unexpectedly")]
    TurnAborted(#[source] tokio::task::JoinError),
}
impl crate::error::Classified for MessengerError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::NoCredential { .. } | Self::NoModel | Self::Endpoint(_) | Self::EndpointPath => {
                ErrorKind::InvalidRequest
            }
            Self::TurnAborted(_) => ErrorKind::Internal,
            Self::BrokerTooOld | Self::VaultUnavailable(_) => ErrorKind::Unavailable,
            Self::Broker(error) => return crate::error::classify(error),
            Self::UnexpectedReply | Self::Malformed { .. } => ErrorKind::Protocol,
            Self::Unauthorized { .. } => ErrorKind::PermissionDenied,
            Self::PollingConflict { .. } => ErrorKind::Conflict,
            Self::Status { .. } | Self::Refused { .. } => ErrorKind::Provider,
            Self::Conversation(error) => return crate::error::classify(error.as_ref()),
            Self::Turn(report) => report.kind,
        })
    }
}
impl MessengerError {
    /// Retrying cannot help until the user changes something.
    fn needs_user(&self) -> bool {
        use a206_broker::{BrokerError, ClientError, UpstreamFailure};
        matches!(
            self,
            Self::NoCredential { .. }
                | Self::Endpoint(_)
                | Self::EndpointPath
                | Self::BrokerTooOld
                | Self::Unauthorized { .. }
                | Self::PollingConflict { .. }
                // The stored credential is missing, refused for this
                // destination or not a bot token: the same one will fail again.
                | Self::Broker(ClientError::Broker(
                    BrokerError::NotFound
                        | BrokerError::Denied(_)
                        | BrokerError::Upstream(UpstreamFailure::InvalidBotToken)
                ))
        )
    }
    /// The pairing ended (vault locked or re-paired): wait for a new one.
    fn lost_pairing(&self) -> bool {
        use a206_broker::{BrokerError, ClientError};
        matches!(
            self,
            Self::Broker(ClientError::Broker(
                BrokerError::Locked | BrokerError::GatewayNotPaired
            ))
        )
    }
}
type Result<T, E = MessengerError> = std::result::Result<T, E>;

/// A message from a chat, before the allow-list is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Inbound {
    pub chat: String,
    /// The sender's id, when the service names one.
    pub sender: Option<String>,
    /// Sent in a group, channel or other chat with several members.
    pub group: bool,
    /// The chat's title or the sender's name: untrusted display text.
    pub title: Option<String>,
    pub text: String,
}

/// What a messenger needs from the gateway and the run engine. The gateway
/// implements it with its ordinary thread commands, so a chat turn is the
/// same operation as a turn typed into the app.
pub trait Host: Clone + Send + Sync + 'static {
    /// The profile directory, where the vault broker listens.
    fn root(&self) -> PathBuf;
    /// The current broker pairing, if the vault is unlocked and paired.
    fn pairing(&self) -> Option<BrokerPairing>;
    /// Changes whenever the gateway receives a new pairing.
    fn pairings(&self) -> watch::Receiver<u64>;
    /// The thread bound to `chat`, created on its first message with the
    /// messenger's mode and model.
    fn thread_for(&self, config: &MessengerConfig, chat: &ChatBinding, title: &str) -> Result<Id>;
    /// Run one user turn in `thread`; the reply text to deliver, if the turn
    /// produced one.
    fn converse(
        &self,
        thread: Id,
        text: String,
    ) -> impl Future<Output = Result<Option<String>>> + Send;
    /// Change the messenger's published status.
    fn update(&self, name: &str, change: impl FnOnce(&mut MessengerStatus) + Send);
}

/// A configured messenger and whether it should run now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desired {
    pub config: MessengerConfig,
    /// Enabled and not disconnected by the user.
    pub run: bool,
}

/// Delays between failed attempts.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    pub initial: Duration,
    pub max: Duration,
}
impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(60),
        }
    }
}

struct Running {
    config: MessengerConfig,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

/// Run every desired messenger until `stop`, restarting one whenever its
/// configuration changes. Cancel-safe: stopping awaits every task.
pub async fn supervise<H: Host>(
    host: H,
    mut desired: watch::Receiver<Vec<Desired>>,
    backoff: Backoff,
    stop: CancellationToken,
) {
    let mut running: BTreeMap<String, Running> = BTreeMap::new();
    loop {
        let wanted = desired.borrow_and_update().clone();
        let mut keep = BTreeMap::new();
        for entry in wanted {
            let name = entry.config.name.clone();
            match running.remove(&name) {
                // A task that ended on a failure needing the user is
                // restarted by any change, such as a connect request.
                Some(current)
                    if entry.run
                        && current.config == entry.config
                        && !current.task.is_finished() =>
                {
                    keep.insert(name, current);
                }
                previous => {
                    if let Some(previous) = previous {
                        finish(previous).await;
                    }
                    if entry.run {
                        let cancel = stop.child_token();
                        let task = tokio::spawn(run(
                            host.clone(),
                            entry.config.clone(),
                            backoff,
                            cancel.clone(),
                        ));
                        keep.insert(
                            name,
                            Running {
                                config: entry.config,
                                cancel,
                                task,
                            },
                        );
                    } else {
                        host.update(&name, |status| {
                            status.state = MessengerState::Stopped;
                        });
                    }
                }
            }
        }
        // Entries removed from settings.
        for (_, previous) in std::mem::replace(&mut running, keep) {
            finish(previous).await;
        }
        tokio::select! {
            changed = desired.changed() => if changed.is_err() { break },
            _ = stop.cancelled() => break,
        }
    }
    for (_, current) in running {
        finish(current).await;
    }
}

async fn finish(running: Running) {
    running.cancel.cancel();
    if let Err(error) = running.task.await {
        crate::error::handle("messenger", &error);
    }
}

/// One messenger's life: wait for a pairing, connect, then receive and answer
/// until cancelled or a failure needs the user.
async fn run<H: Host>(
    host: H,
    config: MessengerConfig,
    backoff: Backoff,
    cancel: CancellationToken,
) {
    let name = config.name.clone();
    let span = tracing::info_span!("messenger", messenger = %name);
    let work = async {
        let mut delay = backoff.initial;
        // Updates up to here are handled; kept across sessions so a retry
        // does not ask the service to deliver them again.
        let mut offset = None;
        let mut pairings = host.pairings();
        loop {
            // Mark the pairing in hand as seen, so waiting below wakes only
            // for a newer one, even if it arrived while this one was in use.
            pairings.borrow_and_update();
            let Some(pairing) = host.pairing() else {
                host.update(&name, |s| s.state = MessengerState::WaitingForVault);
                if pairings.changed().await.is_err() {
                    return;
                }
                continue;
            };
            host.update(&name, |s| s.state = MessengerState::Connecting);
            let result = session(&host, &config, pairing, &mut delay, &mut offset, backoff).await;
            let Err(error) = result else { return };
            let report = crate::error::handle("messenger", &error);
            host.update(&name, |s| {
                s.state = MessengerState::Failed;
                s.failure = Some(report);
            });
            if error.needs_user() {
                return;
            }
            if error.lost_pairing() {
                host.update(&name, |s| s.state = MessengerState::WaitingForVault);
                // Wait for the next pairing rather than retrying a void one.
                if pairings.changed().await.is_err() {
                    return;
                }
                continue;
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(backoff.max);
        }
    };
    tokio::select! {
        // A failure that needs the user stays reported as Failed.
        () = tracing::Instrument::instrument(work, span) => {}
        () = cancel.cancelled() => {
            host.update(&name, |s| s.state = MessengerState::Stopped);
        }
    }
}

/// Connect and serve until an error. Resets `delay` once connected and
/// advances `offset` past each update as its handling starts, so a message
/// is handled at most once while the gateway runs.
async fn session<H: Host>(
    host: &H,
    config: &MessengerConfig,
    pairing: BrokerPairing,
    delay: &mut Duration,
    offset: &mut Option<i64>,
    backoff: Backoff,
) -> Result<()> {
    let api = Api::new(host.root(), pairing, config)?;
    let mut adapter = match config.kind {
        MessengerKind::Telegram => telegram::Telegram::new(api),
    };
    let account = adapter.connect().await?;
    host.update(&config.name, |s| {
        s.state = MessengerState::Connected;
        s.account = account;
        s.failure = None;
    });
    *delay = backoff.initial;
    loop {
        let batch = adapter.receive(*offset).await?;
        for (update, inbound) in batch.updates {
            *offset = Some(update.saturating_add(1));
            if let Some(inbound) = inbound {
                deliver(host, config, &adapter, inbound).await?;
            }
        }
    }
}

/// Route one message: refuse it, or run a turn in the chat's thread and send
/// the reply back. A failed turn is reported to the chat and the status, and
/// does not stop the messenger.
async fn deliver<H: Host>(
    host: &H,
    config: &MessengerConfig,
    adapter: &telegram::Telegram,
    inbound: Inbound,
) -> Result<()> {
    if !config.accepts(&inbound.chat, inbound.sender.as_deref(), inbound.group) {
        // Name the sender only when the chat itself is allowed.
        let sender = inbound
            .sender
            .as_deref()
            .filter(|_| config.allowed_chats.contains(&inbound.chat));
        tracing::info!(chat = %inbound.chat, sender, "refused a message that is not allowed");
        let at = crate::provider::now();
        host.update(&config.name, |s| {
            s.refuse(&inbound.chat, sender, inbound.title, at)
        });
        return Ok(());
    }
    let binding = ChatBinding {
        messenger: config.name.clone(),
        chat: inbound.chat.clone(),
    };
    let title = inbound
        .title
        .clone()
        .unwrap_or_else(|| format!("{} chat {}", config.kind.label(), inbound.chat));
    let turn = async {
        if config.model.is_none() {
            return Err(MessengerError::NoModel);
        }
        let thread = host.thread_for(config, &binding, &title)?;
        host.converse(thread, inbound.text).await
    };
    match turn.await {
        Ok(Some(reply)) => adapter.send(&inbound.chat, &reply).await,
        Ok(None) => Ok(()),
        Err(error) => {
            let report = crate::error::handle("messenger_turn", &error);
            // The chat is where this turn started, so its failure is shown
            // there too, as display text.
            let notice = format!("Agent 206 could not answer: {}", report.detail());
            host.update(&config.name, |s| s.failure = Some(report));
            adapter.send(&inbound.chat, &notice).await
        }
    }
}

/// Destination-bound requests through the vault broker for one messenger.
pub(crate) struct Api {
    root: PathBuf,
    pairing: BrokerPairing,
    credential: String,
    base: url::Url,
}
impl Api {
    fn new(root: PathBuf, pairing: BrokerPairing, config: &MessengerConfig) -> Result<Self> {
        let base =
            a206_core::providers::base_url(config.endpoint()).map_err(MessengerError::Endpoint)?;
        if base.path() != "/" {
            return Err(MessengerError::EndpointPath);
        }
        let credential = config
            .credential
            .clone()
            .ok_or_else(|| MessengerError::NoCredential {
                origin: base.origin().ascii_serialization(),
            })?;
        Ok(Self {
            root,
            pairing,
            credential,
            base,
        })
    }
    /// `relative` resolved under the endpoint, refusing anything that would
    /// leave it.
    fn url(&self, relative: &str) -> Result<url::Url> {
        let url = self
            .base
            .join(relative)
            .map_err(|e| MessengerError::Endpoint(a206_core::providers::EndpointError::Url(e)))?;
        if url.origin() != self.base.origin() || !url.path().starts_with(self.base.path()) {
            return Err(MessengerError::Endpoint(
                a206_core::providers::EndpointError::Escaped,
            ));
        }
        Ok(url)
    }
    /// Send a request with the credential attached by `auth`; the status and
    /// body (empty for an error status, which the broker never relays).
    async fn call(
        &self,
        auth: AuthScheme,
        method: HttpMethod,
        url: url::Url,
        body: Option<Vec<u8>>,
    ) -> Result<(u16, Vec<u8>)> {
        let mut client = a206_broker::Client::connect(&self.root, Role::Gateway)
            .await
            .map_err(MessengerError::VaultUnavailable)?;
        if client.version() < a206_broker::MESSENGER_VERSION {
            return Err(MessengerError::BrokerTooOld);
        }
        client
            .call(Request::GatewayAuth {
                token: a206_broker::WireSecret::new(self.pairing.expose().to_owned()),
            })
            .await
            .map_err(MessengerError::Broker)?;
        let reply = client
            .call(Request::Provider(ProviderCall {
                credential: self.credential.clone(),
                method,
                url: url.to_string(),
                auth,
                anthropic_version: None,
                body,
            }))
            .await
            .map_err(MessengerError::Broker)?;
        match reply {
            BrokerReply::Provider { status, body } => Ok((status, body)),
            _ => Err(MessengerError::UnexpectedReply),
        }
    }
}

#[cfg(test)]
mod tests;
