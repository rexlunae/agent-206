//! Slack adapter: receives events over Socket Mode and answers with the Web
//! API. The broker opens the socket with the app-level token
//! ([`SocketScheme::SlackSocketMode`]; the ticketed socket URL stays in the
//! broker) and attaches the bot token to Web API calls as a bearer token;
//! this side never sees either.
//!
//! The socket runs in its own task, which acknowledges each envelope as it
//! arrives so Slack does not deliver it again, and queues messages for
//! [`Slack::receive`]. Messages sent while the app was disconnected are not
//! delivered.
use super::listener::{self, Listener, Queue};
use super::{Account, Api, Batch, Event, Inbound, MessengerError, Result, split};
use a206_broker::{AuthScheme, HttpMethod, SocketEvent, SocketScheme};
use serde::Deserialize;
use std::collections::{HashSet, VecDeque};

const SERVICE: &str = "Slack";
/// Slack truncates messages past 40,000 characters and advises at most
/// 4,000; split at that, counting UTF-16 code units.
const MAX_MESSAGE_UNITS: usize = 4000;
/// How many recent messages are remembered, so one Slack delivers again
/// (an envelope retried before its acknowledgement arrived) is not answered
/// twice.
const RECENT: usize = 1024;

pub(crate) struct Slack {
    api: Api,
    socket: Option<Listener>,
}

/// A Web API answer: Slack reports failures with `ok: false` and a code.
#[derive(Deserialize)]
struct Answer {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    user: Option<String>,
}

/// A Socket Mode message.
#[derive(Deserialize)]
struct Envelope {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    envelope_id: Option<String>,
    #[serde(default)]
    payload: Option<Payload>,
}

#[derive(Deserialize)]
struct Payload {
    #[serde(default)]
    event: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    channel_type: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    bot_id: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    ts: Option<String>,
}

fn malformed(source: serde_json::Error) -> MessengerError {
    MessengerError::Malformed {
        service: SERVICE,
        source,
    }
}

impl Slack {
    pub(crate) fn new(api: Api) -> Self {
        Self { api, socket: None }
    }

    /// A Web API method, which Slack answers with HTTP 200 and `ok`.
    async fn call(&self, method: &str, body: Option<serde_json::Value>) -> Result<Answer> {
        let body = body
            .map(|body| serde_json::to_vec(&body))
            .transpose()
            .map_err(malformed)?;
        let url = self.api.url_segments(&["api", method])?;
        let (status, bytes) = self
            .api
            .call(AuthScheme::Bearer, HttpMethod::Post, url, body)
            .await?;
        match status {
            200..=299 => {}
            401 => return Err(MessengerError::Unauthorized { service: SERVICE }),
            status => {
                return Err(MessengerError::Status {
                    service: SERVICE,
                    status,
                });
            }
        }
        let answer: Answer = serde_json::from_slice(&bytes).map_err(malformed)?;
        if answer.ok {
            return Ok(answer);
        }
        Err(match answer.error {
            Some(code) if super::UNAUTHORIZED_CODES.contains(&code.as_str()) => {
                MessengerError::Unauthorized { service: SERVICE }
            }
            // Slack's codes are plain identifiers; anything else is dropped.
            Some(code) if plain(&code) => MessengerError::Rejected {
                service: SERVICE,
                code,
            },
            _ => MessengerError::Refused { service: SERVICE },
        })
    }

    /// Check the bot token, name the bot user, and start the socket for it.
    pub(crate) async fn connect(&mut self) -> Result<Account> {
        let me = self.call("auth.test", None).await?;
        let account = Account {
            id: me
                .user_id
                .ok_or(MessengerError::NoAccountId { service: SERVICE })?,
            name: me.user.map(|name| format!("@{name}")),
        };
        // A token for another bot needs its own session.
        if self
            .socket
            .as_ref()
            .is_none_or(|socket| socket.account() != account.id)
        {
            let api = self.api.clone();
            let own = account.id.clone();
            self.socket = Some(Listener::start(SERVICE, account.id.clone(), |queue| {
                serve(api, own, queue)
            }));
        }
        Ok(account)
    }

    /// The messages the socket queued, waiting for at least one.
    pub(crate) async fn receive(&mut self, _cursor: Option<&str>) -> Result<Batch> {
        listener::receive(&mut self.socket, SERVICE).await
    }

    /// Send `text` to `chat`, split to Slack's advised length and escaped so
    /// it cannot mention anyone or link.
    pub(crate) async fn send(&self, chat: &str, text: &str) -> Result<()> {
        // Split before escaping so no part ends inside an entity.
        for part in split(text, MAX_MESSAGE_UNITS) {
            let body = serde_json::json!({
                "channel": chat,
                "text": escape(part),
                "unfurl_links": false,
                "unfurl_media": false,
            });
            self.call("chat.postMessage", Some(body)).await?;
        }
        Ok(())
    }
}

/// Acknowledge envelopes and queue messages until something fails.
async fn serve(api: Api, own: String, queue: Queue) -> MessengerError {
    match session(&api, &own, &queue).await {
        Ok(never) => match never {},
        Err(error) => error,
    }
}

async fn session(api: &Api, own: &str, queue: &Queue) -> Result<std::convert::Infallible> {
    let url = api.url_segments(&["api", "apps.connections.open"])?;
    let mut socket = api.socket(SocketScheme::SlackSocketMode, url, true).await?;
    let mut recent = Recent::default();
    loop {
        let text = match socket.next().await.map_err(MessengerError::Broker)? {
            SocketEvent::Text(text) => text,
            SocketEvent::Closed(code) => {
                return Err(MessengerError::SocketClosed {
                    service: SERVICE,
                    code,
                });
            }
        };
        let envelope: Envelope = serde_json::from_str(&text).map_err(malformed)?;
        // Acknowledge first: Slack redelivers anything unacknowledged after
        // a few seconds, and a turn can take much longer.
        if let Some(id) = &envelope.envelope_id {
            let ack = serde_json::json!({ "envelope_id": id }).to_string();
            socket.send(ack).await.map_err(MessengerError::Broker)?;
        }
        match envelope.kind.as_deref() {
            Some("disconnect") => return Err(MessengerError::Reconnect { service: SERVICE }),
            Some("events_api") => {}
            // Hello, slash commands and interactions carry no messages.
            _ => continue,
        }
        let Some(message) = envelope.payload.and_then(|payload| payload.event) else {
            continue;
        };
        let Some((key, event)) = message_event(message, own) else {
            continue;
        };
        if recent.insert(&key) {
            queue.push(key, event)?;
        }
    }
}

/// Keys of recently queued messages, bounded.
#[derive(Default)]
struct Recent {
    order: VecDeque<String>,
    keys: HashSet<String>,
}
impl Recent {
    /// Whether `key` is new; remembered either way.
    fn insert(&mut self, key: &str) -> bool {
        if !self.keys.insert(key.to_owned()) {
            return false;
        }
        self.order.push_back(key.to_owned());
        if self.order.len() > RECENT
            && let Some(oldest) = self.order.pop_front()
        {
            self.keys.remove(&oldest);
        }
        true
    }
}

/// A message's key and event, unless it is not a person's plain message:
/// the app's own, another bot's, an edit, a join or any other subtype.
fn message_event(message: Message, own: &str) -> Option<(String, Event)> {
    let (Some(channel), Some(ts)) = (message.channel, message.ts) else {
        return None;
    };
    let key = format!("{channel}:{ts}");
    let person = message.kind == "message"
        && message.subtype.is_none()
        && message.bot_id.is_none()
        && message.user.as_deref().is_some_and(|user| user != own);
    let text = message.text.filter(|text| !text.trim().is_empty());
    let (true, Some(text)) = (person, text) else {
        return Some((key, Event::Ignored));
    };
    Some((
        key,
        Event::Message(Inbound {
            chat: channel,
            sender: message.user,
            // Only a direct message is private.
            group: message.channel_type.as_deref() != Some("im"),
            title: None,
            text: unescape(&text),
            encrypted: false,
        }),
    ))
}

/// Slack's three escaped characters, so the model reads what was typed.
fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Escape the characters Slack's markup is built from, so a reply cannot
/// write a mention (`<!channel>`, `<@U…>`) or a link.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn plain(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= 64
        && code
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}
