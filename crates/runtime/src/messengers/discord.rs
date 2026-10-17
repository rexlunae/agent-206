//! Discord adapter: receives messages over the Gateway WebSocket and answers
//! with the REST API. The broker identifies the session with the bot token
//! ([`SocketScheme::DiscordGateway`]) and attaches it to REST requests
//! ([`AuthScheme::DiscordBot`]); this side never sees it.
//!
//! The socket runs in its own task, which keeps heartbeating while a turn is
//! running and queues messages for [`Discord::receive`]. Sessions are not
//! resumed: after a disconnect the broker identifies again, and messages
//! sent while the bot was disconnected are not delivered.
use super::{Account, Api, Batch, Event, Inbound, MessengerError, Result, split};
use a206_broker::{AuthScheme, HttpMethod, SocketEvent, SocketScheme};
use serde::Deserialize;
use std::collections::{BTreeSet, HashMap};
use std::time::Duration;
use tokio::sync::mpsc;

const SERVICE: &str = "Discord";
/// Discord's limit for one message. Splitting counts UTF-16 code units,
/// which are never fewer than characters.
const MAX_MESSAGE_UNITS: usize = 2000;
/// Guild messages, direct messages and message content. Message content
/// is a privileged intent the bot's owner turns on in the developer portal.
const INTENTS: u64 = (1 << 9) | (1 << 12) | (1 << 15);
/// Messages waiting for the gateway; past this the session restarts rather
/// than stop heartbeating.
const BACKLOG: usize = 1024;
/// Most events handed to the supervisor at once.
const MAX_BATCH: usize = 64;
/// Bounds on the heartbeat interval Discord asks for.
const MIN_HEARTBEAT: Duration = Duration::from_secs(1);
const MAX_HEARTBEAT: Duration = Duration::from_secs(600);
/// Close codes after which reconnecting with the same settings cannot help.
const CLOSE_AUTHENTICATION: u16 = 4004;
/// Discord's channel type for a direct message.
const DIRECT_MESSAGE: u8 = 1;
/// How long a channel lookup may hold up a batch.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);
/// Most channel types remembered at once.
const MAX_KNOWN_CHANNELS: usize = 4096;
const CLOSE_INVALID_INTENTS: u16 = 4013;
const CLOSE_DISALLOWED_INTENTS: u16 = 4014;

type Queued = Result<(String, Event)>;

pub(crate) struct Discord {
    api: Api,
    /// Only these channels are ever looked up.
    allowed: BTreeSet<String>,
    gateway: Option<Gateway>,
    private: Private,
}

/// The running socket task and what it has queued.
struct Gateway {
    /// The account the session was opened for.
    account: String,
    events: mpsc::Receiver<Queued>,
    /// A failure that arrived behind queued events, reported next.
    failed: Option<MessengerError>,
    /// Stops the task when the adapter is dropped.
    _stop: tokio_util::sync::DropGuard,
}

#[derive(Deserialize)]
struct User {
    id: String,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    global_name: Option<String>,
    #[serde(default)]
    bot: bool,
}

/// A Gateway message.
#[derive(Deserialize)]
struct Payload {
    op: u8,
    #[serde(default)]
    d: serde_json::Value,
    #[serde(default)]
    s: Option<u64>,
    #[serde(default)]
    t: Option<String>,
}

#[derive(Deserialize)]
struct Hello {
    heartbeat_interval: u64,
}

#[derive(Deserialize)]
struct Ready {
    user: User,
}

#[derive(Deserialize)]
struct Message {
    id: String,
    channel_id: String,
    #[serde(default)]
    guild_id: Option<String>,
    author: User,
    #[serde(default)]
    content: String,
    #[serde(default)]
    webhook_id: Option<String>,
    #[serde(default, rename = "type")]
    kind: u8,
}

fn malformed(source: serde_json::Error) -> MessengerError {
    MessengerError::Malformed {
        service: SERVICE,
        source,
    }
}

impl Discord {
    pub(crate) fn new(api: Api, allowed: BTreeSet<String>) -> Self {
        Self {
            api,
            allowed,
            gateway: None,
            private: Private::default(),
        }
    }

    /// Check the token, name the bot account, and start the socket for it.
    pub(crate) async fn connect(&mut self) -> Result<Account> {
        let url = self.api.url_segments(&["api", "v10", "users", "@me"])?;
        let (status, body) = self
            .api
            .call(AuthScheme::DiscordBot, HttpMethod::Get, url, None)
            .await?;
        check(status)?;
        let me: User = serde_json::from_slice(&body).map_err(malformed)?;
        let account = Account {
            name: me.username.or(me.global_name),
            id: me.id,
        };
        // A token for another bot needs its own session.
        if self
            .gateway
            .as_ref()
            .is_none_or(|gateway| gateway.account != account.id)
        {
            self.gateway = Some(self.start(account.id.clone()));
        }
        Ok(account)
    }

    fn start(&self, account: String) -> Gateway {
        let (sender, events) = mpsc::channel(BACKLOG);
        let stop = tokio_util::sync::CancellationToken::new();
        // Detached: the drop guard cancels it with the adapter, and it holds
        // nothing that needs awaiting. Its failure reaches `receive`.
        tokio::spawn(run(self.api.clone(), sender, stop.clone()));
        Gateway {
            account,
            events,
            failed: None,
            _stop: stop.drop_guard(),
        }
    }

    /// The messages the socket queued, waiting for at least one. A session
    /// failure ends the adapter's session; the next connect starts another.
    ///
    /// Channels are classified here rather than in the socket task, so a
    /// slow lookup never holds up heartbeats. Only allowed channels are
    /// looked up: any other is refused whatever its kind, so a batch waits
    /// at most on channels the user chose, each looked up once.
    pub(crate) async fn receive(&mut self, _cursor: Option<&str>) -> Result<Batch> {
        let mut batch = self.queued().await?;
        for (_, event) in &mut batch.events {
            // Without a guild id, only a channel Discord itself calls a
            // direct message is private.
            if let Event::Message(inbound) = event
                && !inbound.group
            {
                inbound.group = !self.allowed.contains(&inbound.chat)
                    || !self.private.check(&self.api, &inbound.chat).await;
            }
        }
        Ok(batch)
    }

    async fn queued(&mut self) -> Result<Batch> {
        let gateway = self
            .gateway
            .as_mut()
            .ok_or(MessengerError::SocketEnded { service: SERVICE })?;
        let failure = match gateway.failed.take() {
            Some(error) => Err(error),
            None => match gateway.events.recv().await {
                Some(Ok(first)) => Ok(first),
                Some(Err(error)) => Err(error),
                None => Err(MessengerError::SocketEnded { service: SERVICE }),
            },
        };
        let first = match failure {
            Ok(first) => first,
            Err(error) => {
                self.gateway = None;
                return Err(error);
            }
        };
        let mut events = vec![first];
        while events.len() < MAX_BATCH {
            match gateway.events.try_recv() {
                Ok(Ok(event)) => events.push(event),
                Ok(Err(error)) => {
                    gateway.failed = Some(error);
                    break;
                }
                Err(_) => break,
            }
        }
        Ok(Batch {
            events,
            complete: true,
            next: None,
        })
    }

    /// Send `text` to `chat`, split to Discord's limit, with every mention
    /// disabled so a reply cannot ping anyone.
    pub(crate) async fn send(&self, chat: &str, text: &str) -> Result<()> {
        let url = self
            .api
            .url_segments(&["api", "v10", "channels", chat, "messages"])?;
        for part in split(text, MAX_MESSAGE_UNITS) {
            let body = serde_json::json!({
                "content": part,
                "allowed_mentions": { "parse": [] },
            });
            let body = serde_json::to_vec(&body).map_err(malformed)?;
            let (status, _) = self
                .api
                .call(
                    AuthScheme::DiscordBot,
                    HttpMethod::Post,
                    url.clone(),
                    Some(body),
                )
                .await?;
            check(status)?;
        }
        Ok(())
    }
}

fn check(status: u16) -> Result<()> {
    match status {
        200..=299 => Ok(()),
        401 => Err(MessengerError::Unauthorized { service: SERVICE }),
        status => Err(MessengerError::Status {
            service: SERVICE,
            status,
        }),
    }
}

/// Serve the socket until it fails or `stop`, queueing its failure last.
async fn run(api: Api, sender: mpsc::Sender<Queued>, stop: tokio_util::sync::CancellationToken) {
    let error = tokio::select! {
        error = serve(&api, &sender) => error,
        () = stop.cancelled() => return,
    };
    if let Err(unsent) = sender.send(Err(error)).await
        && let Err(error) = unsent.0
    {
        // The adapter is gone, so no session is waiting for this failure.
        tracing::debug!(
            error = &error as &dyn std::error::Error,
            "Discord session ended after its messenger stopped"
        );
    }
}

/// Heartbeat and queue messages until something fails.
async fn serve(api: &Api, sender: &mpsc::Sender<Queued>) -> MessengerError {
    match session(api, sender).await {
        Ok(never) => match never {},
        Err(error) => error,
    }
}

async fn session(api: &Api, sender: &mpsc::Sender<Queued>) -> Result<std::convert::Infallible> {
    let url = api.url_segments(&["api", "v10", "gateway", "bot"])?;
    let mut socket = api
        .socket(SocketScheme::DiscordGateway { intents: INTENTS }, url)
        .await?;
    let SocketEvent::Text(hello) = socket.next().await.map_err(MessengerError::Broker)? else {
        return Err(MessengerError::SocketClosed {
            service: SERVICE,
            code: None,
        });
    };
    let hello: Payload = serde_json::from_str(&hello).map_err(malformed)?;
    let hello: Hello = serde_json::from_value(hello.d).map_err(malformed)?;
    let interval =
        Duration::from_millis(hello.heartbeat_interval).clamp(MIN_HEARTBEAT, MAX_HEARTBEAT);
    // Discord asks for the first heartbeat after a random part of the interval.
    let jitter = interval.mul_f64(f64::from(uuid::Uuid::new_v4().as_u128() as u16) / 65536.0);
    let mut heartbeat = tokio::time::interval_at(tokio::time::Instant::now() + jitter, interval);
    let mut seq: Option<u64> = None;
    let mut acknowledged = true;
    let mut own: Option<String> = None;
    loop {
        let event = tokio::select! {
            _ = heartbeat.tick() => {
                if !acknowledged {
                    return Err(MessengerError::HeartbeatMissed { service: SERVICE });
                }
                acknowledged = false;
                socket.send(beat(seq)).await.map_err(MessengerError::Broker)?;
                continue;
            }
            event = socket.next() => event.map_err(MessengerError::Broker)?,
        };
        let text = match event {
            SocketEvent::Text(text) => text,
            SocketEvent::Closed(code) => return Err(closed(code)),
        };
        let payload: Payload = serde_json::from_str(&text).map_err(malformed)?;
        seq = payload.s.or(seq);
        match payload.op {
            // Heartbeat acknowledged.
            11 => acknowledged = true,
            // Discord asks for a heartbeat now.
            1 => socket
                .send(beat(seq))
                .await
                .map_err(MessengerError::Broker)?,
            7 | 9 => return Err(MessengerError::Reconnect { service: SERVICE }),
            0 => match payload.t.as_deref() {
                Some("READY") => {
                    let ready: Ready = serde_json::from_value(payload.d).map_err(malformed)?;
                    own = Some(ready.user.id);
                }
                Some("MESSAGE_CREATE") => {
                    let message: Message = serde_json::from_value(payload.d).map_err(malformed)?;
                    let key = message.id.clone();
                    let event = message_event(message, own.as_deref());
                    match sender.try_send(Ok((key, event))) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            return Err(MessengerError::Backlog { service: SERVICE });
                        }
                        // The adapter is gone; `run` is being cancelled.
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            return Err(MessengerError::SocketEnded { service: SERVICE });
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
}

/// Channels known to be direct messages, looked up once each.
#[derive(Default)]
struct Private {
    known: HashMap<String, bool>,
}
impl Private {
    /// Whether `channel` is a direct message (type 1). Anything else, or a
    /// lookup that fails or takes too long, counts as a group, so a sender
    /// list is never skipped by mistake. Only answers are remembered.
    async fn check(&mut self, api: &Api, channel: &str) -> bool {
        if let Some(&private) = self.known.get(channel) {
            return private;
        }
        let looked_up = tokio::time::timeout(LOOKUP_TIMEOUT, channel_type(api, channel)).await;
        match looked_up {
            Ok(Ok(kind)) => {
                let private = kind == DIRECT_MESSAGE;
                if self.known.len() >= MAX_KNOWN_CHANNELS {
                    self.known.clear();
                }
                self.known.insert(channel.to_owned(), private);
                private
            }
            Ok(Err(error)) => {
                tracing::warn!(
                    error = &error as &dyn std::error::Error,
                    "could not look up a Discord channel; treating it as a group"
                );
                false
            }
            Err(_) => {
                tracing::warn!("a Discord channel lookup timed out; treating it as a group");
                false
            }
        }
    }
}

#[derive(Deserialize)]
struct Channel {
    #[serde(rename = "type")]
    kind: u8,
}

async fn channel_type(api: &Api, channel: &str) -> Result<u8> {
    let url = api.url_segments(&["api", "v10", "channels", channel])?;
    let (status, body) = api
        .call(AuthScheme::DiscordBot, HttpMethod::Get, url, None)
        .await?;
    check(status)?;
    let channel: Channel = serde_json::from_slice(&body).map_err(malformed)?;
    Ok(channel.kind)
}

fn beat(seq: Option<u64>) -> String {
    serde_json::json!({ "op": 1, "d": seq }).to_string()
}

fn closed(code: Option<u16>) -> MessengerError {
    match code {
        Some(CLOSE_AUTHENTICATION) => MessengerError::Unauthorized { service: SERVICE },
        Some(CLOSE_INVALID_INTENTS | CLOSE_DISALLOWED_INTENTS) => MessengerError::DiscordIntents,
        code => MessengerError::SocketClosed {
            service: SERVICE,
            code,
        },
    }
}

/// A message, unless the bot wrote it, a webhook posted it, or it has no
/// text (a system message, or content withheld without the intent).
fn message_event(message: Message, own: Option<&str>) -> Event {
    let from_self = match own {
        Some(own) => message.author.id == own,
        // Before READY names the bot, no bot's message is taken.
        None => message.author.bot,
    };
    // Default and reply messages; everything else is a system message.
    let text_kind = matches!(message.kind, 0 | 19);
    if from_self || message.webhook_id.is_some() || !text_kind || message.content.trim().is_empty()
    {
        return Event::Ignored;
    }
    let group = message.guild_id.is_some();
    let title = (!group)
        .then(|| message.author.global_name.or(message.author.username))
        .flatten();
    Event::Message(Inbound {
        chat: message.channel_id,
        sender: Some(message.author.id),
        group,
        title,
        text: message.content,
        encrypted: false,
    })
}
