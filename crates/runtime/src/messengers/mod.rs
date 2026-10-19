//! Messengers: chat services that reach threads through the gateway.
//!
//! Each enabled messenger runs as one supervised task. It spends its bot or
//! access token only through the vault broker, for the service's API origin,
//! so the gateway never holds the value. A message from a chat the user has not
//! allowed is refused and listed in the messenger's status; an allowed chat
//! is bound to one thread, and each message becomes one ordinary turn there
//! through [`Host::converse`], under that thread's mode, grants and approvals.
//! A person in an allowed chat can therefore do whatever the user could do
//! by typing into that thread, and nothing more.
mod beeper;
mod discord;
mod irc;
mod listener;
mod matrix;
mod signal;
mod slack;
mod telegram;
pub(crate) mod webhook;

use a206_broker::{AuthScheme, HttpMethod, ProviderCall, Reply as BrokerReply, Request, Role};
use a206_broker::{SocketCall, SocketOpen, SocketScheme};
use a206_core::{BrokerPairing, ChatBinding, ErrorKind, Id, MessengerConfig, MessengerKind};
use a206_core::{MessengerState, MessengerStatus};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// Why a messenger could not connect, receive or deliver.
#[derive(Debug, thiserror::Error)]
pub enum MessengerError {
    #[error(
        "{service} sync exceeds the room, event or pagination budget; the cursor was not advanced"
    )]
    SyncBudget { service: &'static str },
    #[error(
        "{service} returned an incomplete timeline that could not be recovered; the cursor was not advanced"
    )]
    SyncGap { service: &'static str },
    #[error("the messenger has no vault credential; add its token and bind it to {origin}")]
    NoCredential { origin: String },
    #[error(
        "the messenger has no app-level token; store it in the vault bound to {origin} and set it as the app credential"
    )]
    NoAppCredential { origin: String },
    #[error("{service} has no official server; set the messenger's endpoint to yours")]
    NoEndpoint { service: &'static str },
    #[error("the IRC server is not allowed")]
    IrcServer(#[source] a206_core::messengers::IrcServerError),
    #[error(
        "the messenger has no login; set it to the account it signs in as (IRC: the registered nickname; Signal: the number signal-cli serves)"
    )]
    NoLogin,
    #[error("the signal-cli daemon endpoint is not allowed")]
    SignalDaemon(#[source] a206_core::messengers::SignalDaemonError),
    #[error("could not reach the signal-cli daemon; start it, or check the messenger's endpoint")]
    SignalUnreachable {
        #[source]
        source: std::io::Error,
    },
    #[error(
        "the signal-cli daemon does not serve {account}; register or link it there, and run the daemon without -a so it serves every account"
    )]
    SignalAccount { account: String },
    #[error("signal-cli refused the request ({code}: {message})")]
    SignalRefused { code: i64, message: String },
    #[error("signal-cli did not answer in time")]
    SignalTimeout,
    #[error("{service} sent a message larger than Agent 206 reads")]
    TooLarge { service: &'static str },
    #[error(
        "the IRC server does not offer SASL sign-in and account tags, which Agent 206 needs to know who sent each message ({code})"
    )]
    IrcCapabilities { code: String },
    #[error(
        "{chat} is no longer online under the nickname it wrote from, so the reply was not sent"
    )]
    RecipientGone { chat: String },
    #[error("the messenger endpoint is not allowed")]
    Endpoint(#[source] a206_core::providers::EndpointError),
    #[error("a messenger endpoint is an origin, without a path")]
    EndpointPath,
    #[error(
        "the running vault broker predates this messenger; stop it with `agent206 vault stop` and unlock again"
    )]
    BrokerTooOld,
    #[error("could not reach the vault broker")]
    VaultUnavailable(#[source] a206_broker::ClientError),
    #[error("the vault broker refused the request")]
    Broker(#[source] a206_broker::ClientError),
    #[error("the vault broker sent an unexpected reply")]
    UnexpectedReply,
    #[error("{service} rejected the token; store a valid one in the vault")]
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
    #[error("{service} did not identify the bot account")]
    NoAccountId { service: &'static str },
    #[error("Beeper Desktop has no chat accounts connected yet; add one in Beeper")]
    BeeperNoAccounts,
    #[error("{service} messengers only send; there is nothing to connect")]
    SendOnly { service: &'static str },
    #[error("the gateway is not paired with an unlocked vault; unlock the vault and try again")]
    NoPairing,
    #[error("{destination} is not an enabled webhook or an allowed chat of a running messenger")]
    NoDestination { destination: String },
    #[error("a model cannot post to {service} chats outside a reply yet")]
    NoChatPosts { service: &'static str },
    #[error("messenger {name} was disabled, disconnected or changed, so nothing more was sent")]
    Withdrawn { name: String },
    #[error("the message is empty")]
    EmptyMessage,
    #[error(
        "the message is longer than this destination takes ({most} characters); send a shorter one"
    )]
    MessageTooLong { most: usize },
    #[error("the message takes more than {most} IRC lines; send a shorter one")]
    TooManyLines { most: usize },
    #[error("{sent} of {total} parts were sent before one failed")]
    PartlySent {
        sent: usize,
        total: usize,
        #[source]
        source: Box<MessengerError>,
    },
    #[error(
        "room {chat} is end-to-end encrypted, which Agent 206 does not support yet; it neither reads nor answers there"
    )]
    EncryptedRoom { chat: String },
    #[error("{service} closed the connection{}", code.map(|code| format!(" with code {code}")).unwrap_or_default())]
    SocketClosed {
        service: &'static str,
        code: Option<u16>,
    },
    #[error("the {service} connection ended unexpectedly")]
    SocketEnded { service: &'static str },
    #[error("{service} stopped acknowledging heartbeats")]
    HeartbeatMissed { service: &'static str },
    #[error("{service} asked the bot to reconnect")]
    Reconnect { service: &'static str },
    #[error("too many {service} messages were waiting; reconnecting")]
    Backlog { service: &'static str },
    #[error(
        "Discord refused the bot's intents; turn on the Message Content intent for the bot in the Discord developer portal"
    )]
    DiscordIntents,
    #[error("{service} refused the connection ({code})")]
    Rejected { service: &'static str, code: String },
    #[error("choose a model for this messenger before allowing chats")]
    NoModel,
    #[error("the thread could not answer the message")]
    Conversation(#[source] Box<crate::gateway::GatewayError>),
    #[error("the thread answered with an error")]
    Turn(#[source] a206_core::ErrorReport),
    #[error("the turn stopped unexpectedly")]
    TurnAborted(#[source] tokio::task::JoinError),
    #[error("where the messenger stopped reading could not be saved or read")]
    Cursor(#[source] Box<crate::gateway::GatewayError>),
    #[error(
        "messages that arrived while the messenger was stopped could not be read; it answers new messages from now on"
    )]
    Resume(#[source] Box<MessengerError>),
}
impl crate::error::Classified for MessengerError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::SyncBudget { .. } | Self::SyncGap { .. } => ErrorKind::Protocol,
            Self::NoCredential { .. }
            | Self::NoAppCredential { .. }
            | Self::NoEndpoint { .. }
            | Self::IrcServer(_)
            | Self::SignalDaemon(_)
            | Self::SignalAccount { .. }
            | Self::NoLogin
            | Self::NoModel
            | Self::Endpoint(_)
            | Self::EndpointPath
            | Self::SendOnly { .. }
            | Self::NoChatPosts { .. }
            | Self::EmptyMessage
            | Self::MessageTooLong { .. }
            | Self::TooManyLines { .. } => ErrorKind::InvalidRequest,
            Self::NoDestination { .. } => ErrorKind::NotFound,
            Self::NoPairing => ErrorKind::Unavailable,
            Self::Withdrawn { .. } => ErrorKind::Cancelled,
            Self::PartlySent { source, .. } => return crate::error::classify(source.as_ref()),
            Self::TurnAborted(_) => ErrorKind::Internal,
            Self::BrokerTooOld | Self::VaultUnavailable(_) => ErrorKind::Unavailable,
            Self::Broker(error) => return crate::error::classify(error),
            Self::UnexpectedReply | Self::Malformed { .. } | Self::NoAccountId { .. } => {
                ErrorKind::Protocol
            }
            // Answering in plain text there would downgrade the room.
            Self::Unauthorized { .. } | Self::EncryptedRoom { .. } => ErrorKind::PermissionDenied,
            Self::PollingConflict { .. } => ErrorKind::Conflict,
            Self::SocketClosed { .. }
            | Self::SocketEnded { .. }
            | Self::HeartbeatMissed { .. }
            | Self::Reconnect { .. }
            | Self::Backlog { .. } => ErrorKind::Unavailable,
            Self::DiscordIntents => ErrorKind::PermissionDenied,
            Self::Rejected { .. } | Self::IrcCapabilities { .. } => ErrorKind::Provider,
            Self::RecipientGone { .. }
            | Self::SignalUnreachable { .. }
            | Self::SignalTimeout
            | Self::BeeperNoAccounts => ErrorKind::Unavailable,
            Self::SignalRefused { .. } => ErrorKind::Provider,
            Self::TooLarge { .. } => ErrorKind::Protocol,
            Self::Status { .. } | Self::Refused { .. } => ErrorKind::Provider,
            Self::Conversation(error) | Self::Cursor(error) => {
                return crate::error::classify(error.as_ref());
            }
            Self::Resume(error) => return crate::error::classify(error.as_ref()),
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
                | Self::NoAppCredential { .. }
                | Self::NoEndpoint { .. }
                | Self::IrcServer(_)
                | Self::SignalDaemon(_)
                | Self::SignalAccount { .. }
                | Self::NoLogin
                | Self::IrcCapabilities { .. }
                | Self::Endpoint(_)
                | Self::EndpointPath
                | Self::BrokerTooOld
                | Self::Unauthorized { .. }
                | Self::PollingConflict { .. }
                | Self::DiscordIntents
                // The stored credential is missing, refused for this
                // destination or not a bot token: the same one will fail again.
                | Self::Broker(ClientError::Broker(
                    BrokerError::NotFound
                        | BrokerError::Denied(_)
                        | BrokerError::Upstream(
                            UpstreamFailure::InvalidBotToken | UpstreamFailure::InvalidPassword
                        )
                ))
        )
    }
    /// A saved cursor cannot be resumed: the gap since it is beyond what
    /// one batch reads, or the service no longer accepts it.
    fn stale_cursor(&self) -> bool {
        matches!(
            self,
            Self::SyncBudget { .. } | Self::SyncGap { .. } | Self::Status { status: 400, .. }
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
    /// End-to-end encrypted, so `text` is empty: it cannot be read here, and
    /// a plain-text answer would downgrade the chat.
    pub encrypted: bool,
}

/// Something a poll delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Event {
    /// Room discovery only, without authority to join or start a turn.
    Discovered {
        chat: String,
        title: Option<String>,
    },
    Message(Inbound),
    /// An invitation to join `chat`, accepted only if the chat is allowed.
    Invite {
        chat: String,
        title: Option<String>,
    },
    /// Nothing to handle (a service event without a text message).
    Ignored,
}

/// One poll's events in order. Each has a key unique within its batch, so a
/// batch delivered again after a failure skips what was already handled.
pub(crate) struct Batch {
    pub events: Vec<(String, Event)>,
    /// False for an intermediate discovery chunk. Retain deduplication keys
    /// until the complete upstream batch is acknowledged, even after retries.
    pub complete: bool,
    /// The cursor that confirms this whole batch.
    pub next: Option<String>,
}

/// The account the credential currently names. Cursors belong to one
/// account, so a cursor is only meaningful for the same `id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Account {
    pub id: String,
    pub name: Option<String>,
}

/// A connected chat service.
enum Adapter {
    Telegram(telegram::Telegram),
    Matrix(matrix::Matrix),
    Discord(discord::Discord),
    Slack(slack::Slack),
    Irc(irc::Irc),
    Signal(signal::Signal),
    Beeper(beeper::Beeper),
}
impl Adapter {
    /// The adapter for `config`. Kinds that spend a vault credential talk
    /// through the broker; Signal talks to the user's own signal-cli.
    fn open(root: PathBuf, pairing: BrokerPairing, config: &MessengerConfig) -> Result<Self> {
        if config.kind.is_signal() {
            return Ok(Self::Signal(signal::Signal::new(
                config.endpoint(),
                config.login.clone(),
            )));
        }
        if config.kind.is_send_only() {
            return Err(MessengerError::SendOnly {
                service: config.kind.label(),
            });
        }
        let api = Api::new(root, pairing, config)?;
        Ok(match config.kind {
            MessengerKind::Telegram => Self::Telegram(telegram::Telegram::new(api)),
            MessengerKind::Matrix => {
                Self::Matrix(matrix::Matrix::new(api, config.allowed_chats.clone()))
            }
            MessengerKind::Discord => {
                Self::Discord(discord::Discord::new(api, config.allowed_chats.clone()))
            }
            MessengerKind::Slack => Self::Slack(slack::Slack::new(api)),
            MessengerKind::Irc => Self::Irc(irc::Irc::new(
                api,
                config.login.clone(),
                config.allowed_chats.clone(),
            )),
            MessengerKind::Signal => {
                Self::Signal(signal::Signal::new(config.endpoint(), config.login.clone()))
            }
            MessengerKind::Beeper => {
                Self::Beeper(beeper::Beeper::new(api, config.allowed_chats.clone()))
            }
            MessengerKind::Webhook => {
                return Err(MessengerError::SendOnly {
                    service: config.kind.label(),
                });
            }
        })
    }
    /// Check the credential and name the account.
    async fn connect(&mut self) -> Result<Account> {
        match self {
            Self::Telegram(adapter) => adapter.connect().await,
            Self::Matrix(adapter) => adapter.connect().await,
            Self::Discord(adapter) => adapter.connect().await,
            Self::Slack(adapter) => adapter.connect().await,
            Self::Irc(adapter) => adapter.connect().await,
            Self::Signal(adapter) => adapter.connect().await,
            Self::Beeper(adapter) => adapter.connect().await,
        }
    }
    /// Wait for the next batch after `cursor`.
    async fn receive(&mut self, cursor: Option<&str>) -> Result<Batch> {
        match self {
            Self::Telegram(adapter) => adapter.receive(cursor).await,
            Self::Matrix(adapter) => adapter.receive(cursor).await,
            Self::Discord(adapter) => adapter.receive(cursor).await,
            Self::Slack(adapter) => adapter.receive(cursor).await,
            Self::Irc(adapter) => adapter.receive(cursor).await,
            Self::Signal(adapter) => adapter.receive(cursor).await,
            Self::Beeper(adapter) => adapter.receive(cursor).await,
        }
    }
    /// Fails with [`MessengerError::EncryptedRoom`] if an answer could not be
    /// sent to `chat` as it is now.
    async fn check_chat(&self, chat: &str) -> Result<()> {
        match self {
            Self::Telegram(_)
            | Self::Discord(_)
            | Self::Slack(_)
            | Self::Irc(_)
            | Self::Signal(_)
            | Self::Beeper(_) => Ok(()),
            Self::Matrix(adapter) => adapter.check_room(chat).await,
        }
    }
    async fn join(&self, chat: &str) -> Result<()> {
        match self {
            // Telegram, Discord and Slack add bots to chats themselves, and
            // signal-cli and Beeper join groups for the account; none report
            // invitations here.
            Self::Telegram(_)
            | Self::Discord(_)
            | Self::Slack(_)
            | Self::Signal(_)
            | Self::Beeper(_) => Ok(()),
            Self::Matrix(adapter) => adapter.join(chat).await,
            Self::Irc(adapter) => adapter.join(chat).await,
        }
    }
    /// How `send_message` posts through this adapter's running session,
    /// for kinds that send over the session's own connection.
    fn poster(&self, config: &MessengerConfig) -> Option<Poster> {
        let link = match self {
            Self::Irc(adapter) => Link::Irc(adapter.poster()?),
            Self::Signal(adapter) => Link::Signal(adapter.poster()?),
            Self::Telegram(_)
            | Self::Matrix(_)
            | Self::Discord(_)
            | Self::Slack(_)
            | Self::Beeper(_) => return None,
        };
        Some(Poster {
            config: Arc::new(config.clone()),
            link,
        })
    }
    async fn send(&self, chat: &str, text: &str) -> Result<()> {
        match self {
            Self::Telegram(adapter) => adapter.send(chat, text).await,
            Self::Matrix(adapter) => adapter.send(chat, text).await,
            Self::Discord(adapter) => adapter.send(chat, text).await,
            Self::Slack(adapter) => adapter.send(chat, text).await,
            Self::Irc(adapter) => adapter.send(chat, text).await,
            Self::Signal(adapter) => adapter.send(chat, text).await,
            Self::Beeper(adapter) => adapter.send(chat, text).await,
        }
    }
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
    /// Offer the running session of messenger `name` to `send_message`, or
    /// withdraw it with `None` once the messenger stops.
    fn post_through(&self, name: &str, poster: Option<Poster>);
    /// Where messenger `name` last saved its reading position.
    fn saved_cursor(&self, name: &str) -> Result<Option<SavedCursor>>;
    /// Save where messenger `name` stands, so a restart resumes there.
    fn save_cursor(&self, name: &str, cursor: &SavedCursor) -> Result<()>;
}

/// A messenger's reading position as the gateway keeps it across restarts:
/// the cursor that confirms what was handled, and the events of the batch
/// after it whose handling already started. It is only resumed by the same
/// kind of messenger for the same account.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SavedCursor {
    pub kind: MessengerKind,
    pub account: String,
    pub next: Option<String>,
    pub handled: BTreeSet<String>,
}

/// How `send_message` posts to an IRC or Signal chat: through the
/// messenger's running session, which holds the only connection, for the
/// configuration that session runs with.
#[derive(Clone)]
pub struct Poster {
    config: Arc<MessengerConfig>,
    link: Link,
}
#[derive(Clone)]
enum Link {
    Irc(irc::Poster),
    Signal(signal::Poster),
}
impl Poster {
    /// The configuration the session runs with.
    pub fn config(&self) -> &MessengerConfig {
        &self.config
    }
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
                    if entry.run && entry.config.kind.is_send_only() {
                        // Nothing to connect: it sends when a model's
                        // approved call asks it to.
                        host.update(&name, |status| {
                            status.state = MessengerState::Ready;
                            status.failure = None;
                        });
                    } else if entry.run {
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
        let mut carried = Carried::default();
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
            let result = session(&host, &config, pairing, &mut delay, &mut carried, backoff).await;
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
    // The supervisor awaits this before starting the messenger again, so a
    // newer session's poster is never withdrawn here.
    host.post_through(&name, None);
}

/// How far one account's updates are handled. Cursors are per account, and
/// a cursor confirms everything before it, so it is never sent for another.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cursor {
    account: String,
    next: Option<String>,
    /// Keys of the events handled from the batch after `next`, should that
    /// batch be delivered again.
    handled: BTreeSet<String>,
    /// Read from the gateway's state and not yet accepted by the service.
    restored: bool,
}

impl Cursor {
    /// The cursor to poll `account` with: this one if it is the same
    /// account, otherwise a fresh one that confirms nothing.
    fn for_account(current: Option<Self>, account: &str) -> Self {
        current
            .filter(|cursor| cursor.account == account)
            .unwrap_or_else(|| Self::fresh(account))
    }
    fn fresh(account: &str) -> Self {
        Self {
            account: account.to_owned(),
            next: None,
            handled: BTreeSet::new(),
            restored: false,
        }
    }
    fn saved(&self, kind: MessengerKind) -> SavedCursor {
        SavedCursor {
            kind,
            account: self.account.clone(),
            next: self.next.clone(),
            handled: self.handled.clone(),
        }
    }
}

/// Telegram and Matrix keep messages for the account under a cursor that any
/// later session may present, so their position is saved and a restart
/// resumes it. The socket services deliver only while connected, and
/// Beeper's cursor names state held by one session.
fn resumes(kind: MessengerKind) -> bool {
    matches!(kind, MessengerKind::Telegram | MessengerKind::Matrix)
}

/// The position messenger `config` saved, if it is one this messenger can
/// resume. A position that cannot be read is reported, and reading starts
/// over as a session without one would.
fn restore<H: Host>(host: &H, config: &MessengerConfig) -> Option<Cursor> {
    if !resumes(config.kind) {
        return None;
    }
    let saved = match host.saved_cursor(&config.name) {
        Ok(saved) => saved?,
        Err(error) => {
            report(host, config, &error);
            return None;
        }
    };
    (saved.kind == config.kind).then_some(Cursor {
        account: saved.account,
        next: saved.next,
        handled: saved.handled,
        restored: true,
    })
}

/// Save `cursor` for a messenger that resumes. A failure is reported and
/// does not stop the messenger: the position is still kept while it runs,
/// and only a restart before the next successful save may repeat a batch.
fn save<H: Host>(host: &H, config: &MessengerConfig, cursor: &Cursor) {
    if resumes(config.kind)
        && let Err(error) = host.save_cursor(&config.name, &cursor.saved(config.kind))
    {
        report(host, config, &error);
    }
}

/// What one messenger's sessions hand on to the next.
#[derive(Default)]
struct Carried {
    /// Updates up to here are handled; kept across sessions so a retry
    /// does not ask the service to deliver them again.
    cursor: Option<Cursor>,
    /// The adapter, while the pairing it was made for is current, so a
    /// socket's queued events survive a failed account check or reply.
    adapter: Option<(BrokerPairing, Adapter)>,
    /// The saved position was looked up; it is read once per run, after the
    /// first connection, so a failure to read it stays in the status.
    looked_up: bool,
}

/// Connect and serve until an error. Resets `delay` once connected and marks
/// each event handled as its handling starts, so a message is handled at
/// most once while the gateway runs, even if its batch arrives again.
///
/// The credential is looked up by the broker on every request, so its value
/// can be replaced by another account's token at any time. The account is
/// checked again before each poll, and a different one starts from a fresh
/// cursor instead of confirming updates it never delivered.
async fn session<H: Host>(
    host: &H,
    config: &MessengerConfig,
    pairing: BrokerPairing,
    delay: &mut Duration,
    carried: &mut Carried,
    backoff: Backoff,
) -> Result<()> {
    let Carried {
        cursor,
        adapter,
        looked_up,
    } = carried;
    let adapter = match adapter.take() {
        Some((current, kept)) if current == pairing => adapter.insert((current, kept)),
        _ => {
            let opened = Adapter::open(host.root(), pairing.clone(), config)?;
            adapter.insert((pairing, opened))
        }
    };
    let adapter = &mut adapter.1;
    let mut account = adapter.connect().await?;
    if let Some(poster) = adapter.poster(config) {
        host.post_through(&config.name, Some(poster));
    }
    host.update(&config.name, |s| {
        s.state = MessengerState::Connected;
        s.account = account.name.clone();
        s.failure = None;
    });
    *delay = backoff.initial;
    if !std::mem::replace(looked_up, true) {
        *cursor = restore(host, config);
    }
    let mut checked = true;
    loop {
        if !checked {
            let current = adapter.connect().await?;
            if current != account {
                if current.id != account.id {
                    tracing::info!("the messenger credential now names another account");
                }
                host.update(&config.name, |s| s.account = current.name.clone());
                account = current;
            }
        }
        checked = false;
        let current = Cursor::for_account(cursor.take(), &account.id);
        let position = cursor.insert(current);
        let batch = match adapter.receive(position.next.as_deref()).await {
            // Too much arrived while the messenger was stopped, or the
            // service forgot the position: start from now, as a messenger
            // without a saved position does, and say what was skipped.
            Err(error) if position.restored && error.stale_cursor() => {
                report(host, config, &MessengerError::Resume(Box::new(error)));
                *position = Cursor::fresh(&account.id);
                save(host, config, position);
                continue;
            }
            batch => batch?,
        };
        position.restored = false;
        for (key, event) in batch.events {
            if position.handled.insert(key) {
                // Saved before handling starts, so a restart during the turn
                // never answers this event again.
                save(host, config, position);
                deliver(host, config, adapter, event).await?;
            }
        }
        if batch.complete && (position.next != batch.next || !position.handled.is_empty()) {
            position.next = batch.next;
            position.handled.clear();
            save(host, config, position);
        }
    }
}

/// Record a message or invitation that is not allowed in the status.
fn refuse<H: Host>(
    host: &H,
    config: &MessengerConfig,
    chat: &str,
    sender: Option<&str>,
    title: Option<String>,
) {
    // Name the sender only when the chat itself is allowed.
    let sender = sender.filter(|_| config.allowed_chats.contains(chat));
    tracing::info!(chat, sender, "refused a message that is not allowed");
    let at = crate::provider::now();
    host.update(&config.name, |s| s.refuse(chat, sender, title, at));
}

/// Report a failure that does not stop the messenger in its status.
fn report<H: Host>(host: &H, config: &MessengerConfig, error: &MessengerError) -> String {
    let report = crate::error::handle("messenger_turn", error);
    let detail = report.detail();
    host.update(&config.name, |s| s.failure = Some(report));
    detail
}

/// Route one event: refuse it, join an allowed chat, or run a turn in the
/// chat's thread and send the reply back. A failed turn is reported to the
/// chat and the status, and does not stop the messenger. Nothing is ever
/// sent in plain text to an encrypted chat; that is reported in the status
/// only.
async fn deliver<H: Host>(
    host: &H,
    config: &MessengerConfig,
    adapter: &Adapter,
    event: Event,
) -> Result<()> {
    let inbound = match event {
        Event::Discovered { chat, title } => {
            refuse(host, config, &chat, None, title);
            return Ok(());
        }
        Event::Ignored => return Ok(()),
        Event::Invite { chat, title } => {
            if config.allowed_chats.contains(&chat) {
                return adapter.join(&chat).await;
            }
            refuse(host, config, &chat, None, title);
            return Ok(());
        }
        Event::Message(inbound) => inbound,
    };
    if !config.accepts(&inbound.chat, inbound.sender.as_deref(), inbound.group) {
        refuse(
            host,
            config,
            &inbound.chat,
            inbound.sender.as_deref(),
            inbound.title,
        );
        return Ok(());
    }
    let chat = inbound.chat.clone();
    let encrypted = |error: &MessengerError| matches!(error, MessengerError::EncryptedRoom { .. });
    let checked = if inbound.encrypted {
        Err(MessengerError::EncryptedRoom { chat: chat.clone() })
    } else {
        adapter.check_chat(&chat).await
    };
    match checked {
        Err(error) if encrypted(&error) => {
            report(host, config, &error);
            return Ok(());
        }
        checked => checked?,
    }
    let binding = ChatBinding {
        messenger: config.name.clone(),
        chat: chat.clone(),
    };
    let title = inbound
        .title
        .clone()
        .unwrap_or_else(|| format!("{} chat {chat}", config.kind.label()));
    let turn = async {
        if config.model.is_none() {
            return Err(MessengerError::NoModel);
        }
        let thread = host.thread_for(config, &binding, &title)?;
        host.converse(thread, inbound.text).await
    };
    let sent = match turn.await {
        Ok(Some(reply)) => adapter.send(&chat, &reply).await,
        Ok(None) => Ok(()),
        Err(error) => {
            // The chat is where this turn started, so its failure is shown
            // there too, as display text.
            let notice = format!(
                "Agent 206 could not answer: {}",
                report(host, config, &error)
            );
            adapter.send(&chat, &notice).await
        }
    };
    match sent {
        // The chat became encrypted during the turn, or the person it was
        // for no longer holds the nickname: report it and keep listening.
        Err(error)
            if encrypted(&error) || matches!(error, MessengerError::RecipientGone { .. }) =>
        {
            report(host, config, &error);
            Ok(())
        }
        sent => sent,
    }
}

/// Split `text` into parts of at most `limit` UTF-16 code units, preferring
/// to break after a newline, never inside a character.
fn split(text: &str, limit: usize) -> Vec<&str> {
    let mut parts = vec![];
    let mut rest = text;
    while !rest.is_empty() {
        let mut units = 0;
        let mut end = rest.len();
        let mut newline = None;
        for (index, c) in rest.char_indices() {
            if units + c.len_utf16() > limit {
                end = index;
                break;
            }
            units += c.len_utf16();
            if c == '\n' {
                newline = Some(index + 1);
            }
        }
        if end < rest.len()
            && let Some(newline) = newline
        {
            end = newline;
        }
        let (part, tail) = rest.split_at(end);
        parts.push(part);
        rest = tail;
    }
    parts
}

/// Said at the end of a reply that was cut to a service's part limit.
const CUT_SHORT: &str = "\n(reply cut short; read the rest in Agent 206)";

/// `text` in non-blank parts of at most `limit` UTF-16 units, at most
/// `most` of them; a reply cut at that count ends by saying so, so a
/// partial answer is never sent as if it were whole.
fn bounded_parts(text: &str, limit: usize, most: usize) -> Vec<String> {
    let mut parts: Vec<String> = split(text, limit)
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .map(str::to_owned)
        .collect();
    if parts.len() > most {
        parts.truncate(most);
        let last = parts.pop().unwrap_or_default();
        let kept = split(&last, limit - CUT_SHORT.encode_utf16().count())
            .into_iter()
            .next()
            .unwrap_or_default();
        parts.push(format!("{kept}{CUT_SHORT}"));
    }
    parts
}

/// Most parts a message sent with `send_message` is posted in; a longer
/// message is refused whole rather than cut.
pub const MAX_SENT_PARTS: usize = 8;

/// `text` in the parts of at most `limit` UTF-16 units it is sent in,
/// leaving out any part that would hold only whitespace, or why it cannot
/// be sent whole.
fn sent_parts(text: &str, limit: usize) -> Result<Vec<String>> {
    let parts: Vec<String> = split(text, limit)
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .map(str::to_owned)
        .collect();
    if parts.is_empty() {
        return Err(MessengerError::EmptyMessage);
    }
    if parts.len() > MAX_SENT_PARTS {
        return Err(MessengerError::MessageTooLong {
            most: limit * MAX_SENT_PARTS,
        });
    }
    Ok(parts)
}

/// Send `parts` in order with `send`; the number sent. `withdrawn` is asked
/// before each part: once the user has disabled, disconnected or changed
/// messenger `name`, nothing more is sent. A part that fails or is withheld
/// stops the rest, and the error says how many were already delivered.
async fn send_in_parts<F: Future<Output = Result<()>>>(
    name: &str,
    parts: Vec<String>,
    withdrawn: impl Fn() -> bool,
    send: impl Fn(String) -> F,
) -> Result<usize> {
    let total = parts.len();
    for (sent, part) in parts.into_iter().enumerate() {
        let delivered = if withdrawn() {
            Err(MessengerError::Withdrawn {
                name: name.to_owned(),
            })
        } else {
            send(part).await
        };
        if let Err(error) = delivered {
            return Err(if sent == 0 {
                error
            } else {
                MessengerError::PartlySent {
                    sent,
                    total,
                    source: Box::new(error),
                }
            });
        }
    }
    Ok(total)
}

/// Whether a model can post to chats of `kind` outside a reply: with one
/// request through the broker per part ([`chat_part_limit`]), or for IRC and
/// Signal through the running session ([`post_through`]).
pub fn posts_to_chats(kind: MessengerKind) -> bool {
    chat_part_limit(kind).is_some() || matches!(kind, MessengerKind::Irc | MessengerKind::Signal)
}

/// The longest part `send_message` posts to a chat of `kind` with one
/// request through the broker, or `None` for kinds it does not post to that
/// way. IRC and Signal send over the running messenger's own connection,
/// through [`post_through`].
pub fn chat_part_limit(kind: MessengerKind) -> Option<usize> {
    match kind {
        MessengerKind::Telegram => Some(telegram::MAX_MESSAGE_UNITS),
        MessengerKind::Matrix => Some(matrix::MAX_MESSAGE_UNITS),
        MessengerKind::Discord => Some(discord::MAX_MESSAGE_UNITS),
        MessengerKind::Slack => Some(slack::MAX_MESSAGE_UNITS),
        MessengerKind::Beeper => Some(beeper::MAX_MESSAGE),
        MessengerKind::Irc | MessengerKind::Signal | MessengerKind::Webhook => None,
    }
}

/// Post `text` to `chat` of `config` as a reply there is sent, outside any
/// turn of that chat; the number of parts sent. The caller checks that the
/// chat is allowed and the messenger running; see [`send_in_parts`] for
/// `withdrawn`.
pub(crate) async fn post(
    root: PathBuf,
    pairing: BrokerPairing,
    config: &MessengerConfig,
    chat: &str,
    text: &str,
    withdrawn: impl Fn() -> bool,
) -> Result<usize> {
    let limit = chat_part_limit(config.kind).ok_or(MessengerError::NoChatPosts {
        service: config.kind.label(),
    })?;
    let parts = sent_parts(text, limit)?;
    let adapter = Adapter::open(root, pairing, config)?;
    let adapter = &adapter;
    send_in_parts(&config.name, parts, withdrawn, |part| async move {
        adapter.send(chat, &part).await
    })
    .await
}

/// Post `text` to `chat` through `poster`'s running session; the number of
/// parts sent. IRC posts each line as a part, refusing a message that needs
/// more lines than a reply may have; Signal posts parts of one message each.
/// See [`send_in_parts`] for `withdrawn`.
pub(crate) async fn post_through(
    poster: &Poster,
    chat: &str,
    text: &str,
    withdrawn: impl Fn() -> bool,
) -> Result<usize> {
    let name = &poster.config.name;
    match &poster.link {
        Link::Irc(link) => {
            let lines = irc::post_lines(chat, text)?;
            send_in_parts(name, lines, withdrawn, |line| async move {
                link.send(chat, &line).await
            })
            .await
        }
        Link::Signal(link) => {
            let parts = sent_parts(text, signal::MAX_MESSAGE)?;
            send_in_parts(name, parts, withdrawn, |part| async move {
                link.send(chat, &part).await
            })
            .await
        }
    }
}

/// Destination-bound requests through the vault broker for one messenger.
#[derive(Clone)]
pub(crate) struct Api {
    service: &'static str,
    root: PathBuf,
    pairing: BrokerPairing,
    credential: String,
    /// The app-level token's credential, for kinds that receive with one.
    app_credential: Option<String>,
    base: url::Url,
    /// The exact destination the credentials are bound to.
    origin: String,
}
impl Api {
    fn new(root: PathBuf, pairing: BrokerPairing, config: &MessengerConfig) -> Result<Self> {
        let endpoint = config.endpoint().ok_or(MessengerError::NoEndpoint {
            service: config.kind.label(),
        })?;
        let (base, origin) = if config.kind.is_irc() {
            let server = a206_core::messengers::IrcServer::parse(endpoint)
                .map_err(MessengerError::IrcServer)?;
            let origin = server.origin();
            let base = url::Url::parse(&origin).map_err(|_| {
                MessengerError::IrcServer(a206_core::messengers::IrcServerError::Shape)
            })?;
            (base, origin)
        } else {
            let base =
                a206_core::providers::base_url(endpoint).map_err(MessengerError::Endpoint)?;
            if base.path() != "/" {
                return Err(MessengerError::EndpointPath);
            }
            let origin = base.origin().ascii_serialization();
            (base, origin)
        };
        let credential = config
            .credential
            .clone()
            .ok_or_else(|| MessengerError::NoCredential {
                origin: origin.clone(),
            })?;
        Ok(Self {
            service: config.kind.label(),
            root,
            pairing,
            credential,
            app_credential: config.app_credential.clone(),
            base,
            origin,
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
    /// The endpoint with `segments` appended, each percent-encoded, so an id
    /// cannot change the path's shape.
    fn url_segments(&self, segments: &[&str]) -> Result<url::Url> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|()| MessengerError::Endpoint(a206_core::providers::EndpointError::Shape))?
            .pop_if_empty()
            .extend(segments);
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
        let needed = match method {
            HttpMethod::Put => a206_broker::PUT_VERSION,
            HttpMethod::Get | HttpMethod::Post => a206_broker::MESSENGER_VERSION,
        };
        let needed = match auth {
            AuthScheme::DiscordBot => needed.max(a206_broker::SOCKET_VERSION),
            AuthScheme::WebhookUrl => needed.max(a206_broker::WEBHOOK_VERSION),
            _ => needed,
        };
        let mut client = self.client(needed).await?;
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

    /// A paired gateway connection to a broker that speaks `needed`.
    async fn client(&self, needed: u16) -> Result<a206_broker::Client> {
        let mut client = a206_broker::Client::connect(&self.root, Role::Gateway)
            .await
            .map_err(MessengerError::VaultUnavailable)?;
        if client.version() < needed {
            return Err(MessengerError::BrokerTooOld);
        }
        client
            .call(Request::GatewayAuth {
                token: a206_broker::WireSecret::new(self.pairing.expose().to_owned()),
            })
            .await
            .map_err(MessengerError::Broker)?;
        Ok(client)
    }

    /// Open a WebSocket session named by `url`, with the credential (or,
    /// for `app`, the app-level one) spent by the broker as `scheme` says.
    async fn socket(
        &self,
        scheme: SocketScheme,
        url: url::Url,
        app: bool,
    ) -> Result<a206_broker::Socket> {
        let credential = if app {
            self.app_credential
                .clone()
                .ok_or_else(|| MessengerError::NoAppCredential {
                    origin: self.origin.clone(),
                })?
        } else {
            self.credential.clone()
        };
        let needed = match scheme {
            SocketScheme::IrcSasl { .. } => a206_broker::IRC_VERSION,
            SocketScheme::DiscordGateway { .. } | SocketScheme::SlackSocketMode => {
                a206_broker::SOCKET_VERSION
            }
        };
        let client = self.client(needed).await?;
        let opened = client
            .socket(SocketCall {
                credential,
                url: url.to_string(),
                scheme,
            })
            .await
            .map_err(MessengerError::Broker)?;
        let service = self.service;
        match opened {
            SocketOpen::Opened(socket) => Ok(socket),
            SocketOpen::Refused {
                status: 401 | 403, ..
            } => Err(MessengerError::Unauthorized { service }),
            SocketOpen::Refused {
                code: Some(code), ..
            } if UNAUTHORIZED_CODES.contains(&code.as_str()) => {
                Err(MessengerError::Unauthorized { service })
            }
            SocketOpen::Refused {
                code: Some(code), ..
            } if IRC_CAPABILITY_CODES.contains(&code.as_str()) => {
                Err(MessengerError::IrcCapabilities { code })
            }
            SocketOpen::Refused {
                code: Some(code), ..
            } => Err(MessengerError::Rejected { service, code }),
            SocketOpen::Refused { status, code: None } => {
                Err(MessengerError::Status { service, status })
            }
        }
    }
}

/// Error codes that mean the stored token is wrong for the request: Slack's,
/// and the broker's for a refused IRC sign-in.
const UNAUTHORIZED_CODES: [&str; 7] = [
    "invalid_auth",
    "not_authed",
    "account_inactive",
    "token_revoked",
    "token_expired",
    "not_allowed_token_type",
    "sasl_failed",
];
/// The broker's codes for an IRC server without the sign-in Agent 206 needs.
const IRC_CAPABILITY_CODES: [&str; 2] = ["sasl_unavailable", "account_tag_unavailable"];

#[cfg(test)]
mod tests;
