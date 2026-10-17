//! Matrix client-server API adapter: long-polls `/sync` on the user's
//! homeserver and answers with `m.notice` messages. The broker attaches the
//! bot account's access token as a bearer token for the homeserver origin;
//! this side never sees it.
//!
//! Every room is treated as a group, so a message is accepted only from a
//! listed sender in an allowed room. The bot joins a room only when invited
//! to an allowed one. End-to-end encryption is not supported: encrypted
//! messages cannot be read, and nothing is ever sent to an encrypted room,
//! because a plain-text answer there would downgrade the room's protection.
//!
//! The first sync of a session that starts without a cursor answers nothing:
//! it only marks where to start, so history is never answered. The cursor is
//! kept across reconnects while the gateway runs, but not across restarts.
use super::{Account, Api, Batch, Event, Inbound, MessengerError, Result, split};
use a206_broker::{AuthScheme, HttpMethod};
use serde::Deserialize;
use std::collections::BTreeMap;

const SERVICE: &str = "Matrix";
/// Milliseconds the homeserver holds a sync open; well inside the broker's
/// timeout.
const POLL_MILLISECONDS: u32 = 25_000;
/// Most timeline events one sync returns per room.
const TIMELINE_LIMIT: u32 = 50;
/// Parts a long reply is split into, in UTF-16 code units. Matrix limits an
/// event to 64 KiB including its JSON envelope.
const MAX_MESSAGE_UNITS: usize = 8000;
const CLIENT: [&str; 3] = ["_matrix", "client", "v3"];

pub(crate) struct Matrix {
    api: Api,
    /// The bot's own user id, whose messages are never answered.
    user: Option<String>,
}

#[derive(Deserialize)]
struct WhoAmI {
    #[serde(default)]
    user_id: Option<String>,
}

#[derive(Deserialize)]
struct Sync {
    next_batch: String,
    #[serde(default)]
    rooms: Rooms,
}

#[derive(Default, Deserialize)]
struct Rooms {
    #[serde(default)]
    join: BTreeMap<String, Joined>,
    #[serde(default)]
    invite: BTreeMap<String, Invited>,
}

#[derive(Default, Deserialize)]
struct Joined {
    #[serde(default)]
    timeline: Timeline,
}

#[derive(Default, Deserialize)]
struct Timeline {
    #[serde(default)]
    events: Vec<RoomEvent>,
}

#[derive(Deserialize)]
struct RoomEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    event_id: Option<String>,
    #[serde(default)]
    sender: Option<String>,
    #[serde(default)]
    content: Content,
}

#[derive(Default, Deserialize)]
struct Content {
    #[serde(default)]
    msgtype: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Default, Deserialize)]
struct Invited {
    #[serde(default)]
    invite_state: InviteState,
}

#[derive(Default, Deserialize)]
struct InviteState {
    #[serde(default)]
    events: Vec<RoomEvent>,
}

impl Matrix {
    pub(crate) fn new(api: Api) -> Self {
        Self { api, user: None }
    }

    fn url(&self, path: &[&str]) -> Result<url::Url> {
        let segments: Vec<&str> = CLIENT.iter().chain(path).copied().collect();
        self.api.url_segments(&segments)
    }

    /// Send a request; the status, and the body of a success.
    async fn call(
        &self,
        method: HttpMethod,
        url: url::Url,
        body: Option<serde_json::Value>,
    ) -> Result<(u16, Vec<u8>)> {
        let body = body
            .map(|body| serde_json::to_vec(&body))
            .transpose()
            .map_err(|source| MessengerError::Malformed {
                service: SERVICE,
                source,
            })?;
        self.api.call(AuthScheme::Bearer, method, url, body).await
    }

    async fn request<T: for<'de> Deserialize<'de>>(
        &self,
        method: HttpMethod,
        url: url::Url,
        body: Option<serde_json::Value>,
    ) -> Result<T> {
        let (status, bytes) = self.call(method, url, body).await?;
        success(status)?;
        parse(&bytes)
    }

    /// Check the token and name the bot account.
    pub(crate) async fn connect(&mut self) -> Result<Account> {
        let me: WhoAmI = self
            .request(HttpMethod::Get, self.url(&["account", "whoami"])?, None)
            .await?;
        let id = me
            .user_id
            .ok_or(MessengerError::NoAccountId { service: SERVICE })?;
        self.user = Some(id.clone());
        Ok(Account {
            name: Some(id.clone()),
            id,
        })
    }

    /// Wait for events after `since`. Without one, this is the first sync:
    /// it returns at once, delivers only pending invitations, and its cursor
    /// marks where answering starts.
    pub(crate) async fn receive(&mut self, since: Option<&str>) -> Result<Batch> {
        let filter = serde_json::json!({
            "presence": {"types": []},
            "account_data": {"types": []},
            "room": {
                "account_data": {"types": []},
                "ephemeral": {"types": []},
                "state": {"types": [], "lazy_load_members": true},
                "timeline": {
                    "limit": TIMELINE_LIMIT,
                    "types": ["m.room.message", "m.room.encrypted"],
                },
            },
        });
        let mut url = self.url(&["sync"])?;
        url.query_pairs_mut()
            .append_pair("filter", &filter.to_string())
            .append_pair(
                "timeout",
                &since.map_or(0, |_| POLL_MILLISECONDS).to_string(),
            );
        if let Some(since) = since {
            url.query_pairs_mut().append_pair("since", since);
        }
        let sync: Sync = self.request(HttpMethod::Get, url, None).await?;
        let mut events = vec![];
        for (room, invited) in sync.rooms.invite {
            let title = invited
                .invite_state
                .events
                .iter()
                .find(|e| e.kind == "m.room.name")
                .and_then(|e| e.content.name.clone());
            events.push((
                format!("invite {room}"),
                Event::Invite { chat: room, title },
            ));
        }
        if since.is_some() {
            for (room, joined) in sync.rooms.join {
                for event in joined.timeline.events {
                    let Some(id) = event.event_id.clone() else {
                        continue;
                    };
                    events.push((id, self.event(&room, event)));
                }
            }
        }
        Ok(Batch {
            events,
            next: Some(sync.next_batch),
        })
    }

    fn event(&self, room: &str, event: RoomEvent) -> Event {
        let Some(sender) = event.sender.filter(|s| Some(s) != self.user.as_ref()) else {
            return Event::Ignored;
        };
        let inbound = |text: String, encrypted| {
            Event::Message(Inbound {
                chat: room.to_owned(),
                sender: Some(sender.clone()),
                group: true,
                title: None,
                text,
                encrypted,
            })
        };
        match (event.kind.as_str(), event.content) {
            ("m.room.encrypted", _) => inbound(String::new(), true),
            // Only text: notices are what bots send, and answering them
            // could loop between bots.
            (
                "m.room.message",
                Content {
                    msgtype: Some(msgtype),
                    body: Some(body),
                    ..
                },
            ) if msgtype == "m.text" && !body.trim().is_empty() => inbound(body, false),
            _ => Event::Ignored,
        }
    }

    /// Fails if `room` is end-to-end encrypted: the encryption state event
    /// exists. A room cannot turn encryption off again.
    pub(crate) async fn check_room(&self, room: &str) -> Result<()> {
        let url = self.url(&["rooms", room, "state", "m.room.encryption"])?;
        match self.call(HttpMethod::Get, url, None).await?.0 {
            404 => Ok(()),
            200..=299 => Err(MessengerError::EncryptedRoom {
                chat: room.to_owned(),
            }),
            status => success(status),
        }
    }

    pub(crate) async fn join(&self, room: &str) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                HttpMethod::Post,
                self.url(&["rooms", room, "join"])?,
                Some(serde_json::json!({})),
            )
            .await?;
        Ok(())
    }

    /// Send `text` to `room` as notices, split to stay within an event's
    /// size. The room's encryption is checked first, every time.
    pub(crate) async fn send(&self, room: &str, text: &str) -> Result<()> {
        self.check_room(room).await?;
        for part in split(text, MAX_MESSAGE_UNITS) {
            let transaction = uuid::Uuid::new_v4().simple().to_string();
            let url = self.url(&["rooms", room, "send", "m.room.message", &transaction])?;
            let _: serde_json::Value = self
                .request(
                    HttpMethod::Put,
                    url,
                    Some(serde_json::json!({ "msgtype": "m.notice", "body": part })),
                )
                .await?;
        }
        Ok(())
    }
}

fn success(status: u16) -> Result<()> {
    match status {
        200..=299 => Ok(()),
        401 => Err(MessengerError::Unauthorized { service: SERVICE }),
        status => Err(MessengerError::Status {
            service: SERVICE,
            status,
        }),
    }
}

fn parse<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|source| MessengerError::Malformed {
        service: SERVICE,
        source,
    })
}
