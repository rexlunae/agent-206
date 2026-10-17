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
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const SERVICE: &str = "Matrix";
/// Milliseconds the homeserver holds a sync open; well inside the broker's
/// timeout.
const POLL_MILLISECONDS: u32 = 25_000;
/// Most timeline events one sync returns per room.
const TIMELINE_LIMIT: u32 = 50;
const MAX_ROOMS: usize = 64;
const MAX_EVENTS: usize = 1_000;
const MAX_PAGES: usize = 32;
const MAX_DISCOVERIES: usize = 4_096;
/// Parts a long reply is split into, in UTF-16 code units. Matrix limits an
/// event to 64 KiB including its JSON envelope.
const MAX_MESSAGE_UNITS: usize = 8000;
const CLIENT: [&str; 3] = ["_matrix", "client", "v3"];

pub(crate) struct Matrix {
    api: Api,
    allowed: BTreeSet<String>,
    pending: Option<Pending>,
    /// The bot's own user id, whose messages are never answered.
    user: Option<String>,
}

/// Discovery is drained before authorized effects. Until the final chunk,
/// keep the old cursor: reconnecting can repeat a refusal, never a turn.
struct Pending {
    discoveries: VecDeque<(String, Event)>,
    events: Vec<(String, Event)>,
    previous: Option<String>,
    next: String,
}
impl Pending {
    fn chunk(&mut self) -> Batch {
        let mut events: Vec<_> = self
            .discoveries
            .drain(..self.discoveries.len().min(MAX_ROOMS))
            .collect();
        let next = if self.discoveries.is_empty() {
            events.append(&mut self.events);
            Some(self.next.clone())
        } else {
            self.previous.clone()
        };
        Batch { events, next }
    }
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
    limited: bool,
    #[serde(default)]
    prev_batch: Option<String>,
    #[serde(default)]
    events: Vec<RoomEvent>,
}

#[derive(Deserialize)]
struct Page {
    chunk: Vec<RoomEvent>,
    #[serde(default)]
    end: Option<String>,
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
    #[serde(default, rename = "m.relates_to")]
    relation: Option<Relation>,
    #[serde(default)]
    msgtype: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct Relation {
    #[serde(default)]
    rel_type: Option<String>,
}

/// A full successful state response, scanned without allocating a membership
/// list. The broker already bounds its bytes. Consume the entire sequence:
/// truncated or malformed state must never authorize plaintext.
struct EncryptionState(bool);
impl<'de> Deserialize<'de> for EncryptionState {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = EncryptionState;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a complete room state array")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                #[derive(Deserialize)]
                struct Kind {
                    #[serde(rename = "type")]
                    kind: String,
                }
                let mut encrypted = false;
                while let Some(event) = seq.next_element::<Kind>()? {
                    encrypted |= event.kind == "m.room.encryption";
                }
                Ok(EncryptionState(encrypted))
            }
        }
        deserializer.deserialize_seq(Visitor)
    }
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
    pub(crate) fn new(api: Api, allowed: BTreeSet<String>) -> Self {
        Self {
            api,
            allowed,
            pending: None,
            user: None,
        }
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
        if self.user.as_ref() != Some(&id) {
            self.pending = None;
        }
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
        if let Some(pending) = &mut self.pending {
            if pending.previous.as_deref() == since {
                let batch = pending.chunk();
                if pending.discoveries.is_empty() {
                    self.pending = None;
                }
                return Ok(batch);
            }
            self.pending = None;
        }
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
        let mut sync: Sync = self.request(HttpMethod::Get, url, None).await?;
        let unrelated = sync
            .rooms
            .join
            .keys()
            .chain(sync.rooms.invite.keys())
            .filter(|room| !self.allowed.contains(*room))
            .count();
        if unrelated > MAX_DISCOVERIES {
            return Err(MessengerError::SyncBudget { service: SERVICE });
        }
        let mut discoveries: VecDeque<_> = sync
            .rooms
            .join
            .iter()
            .filter(|(room, _)| !self.allowed.contains(*room))
            .map(|(room, _)| {
                (
                    format!("room {room}"),
                    Event::Discovered {
                        chat: room.clone(),
                        title: None,
                    },
                )
            })
            .collect();
        // Unrelated joined rooms have no delivery authority. Discard them
        // before charging the work budget, so they cannot starve allowed rooms.
        sync.rooms
            .join
            .retain(|room, _| self.allowed.contains(room));
        // Keep bounded discovery of pending, unapproved invitations. These
        // can only reach the user's refused-room list, never a conversation.
        discoveries.extend(
            sync.rooms
                .invite
                .iter()
                .filter(|(room, _)| !self.allowed.contains(*room))
                .map(|(room, invited)| {
                    (
                        format!("invite {room}"),
                        Event::Discovered {
                            chat: room.clone(),
                            title: invited
                                .invite_state
                                .events
                                .iter()
                                .take(50)
                                .find(|e| e.kind == "m.room.name")
                                .and_then(|e| e.content.name.clone()),
                        },
                    )
                }),
        );
        sync.rooms
            .invite
            .retain(|room, _| self.allowed.contains(room));
        if sync
            .rooms
            .join
            .len()
            .saturating_add(sync.rooms.invite.len())
            > MAX_ROOMS
        {
            return Err(MessengerError::SyncBudget { service: SERVICE });
        }
        let mut count = 0;
        for joined in sync.rooms.join.values() {
            charge(&mut count, joined.timeline.events.len())?;
        }
        for invited in sync.rooms.invite.values() {
            charge(
                &mut count,
                invited.invite_state.events.len().saturating_add(1),
            )?;
        }
        let mut pages = 0;
        let mut seen = BTreeSet::new();
        let mut events = Vec::new();
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
        if let Some(since) = since {
            for (room, joined) in sync.rooms.join {
                let mut timeline = joined.timeline;
                if timeline.limited {
                    let to = timeline
                        .prev_batch
                        .as_deref()
                        .ok_or(MessengerError::SyncGap { service: SERVICE })?;
                    let mut recovered = self
                        .backfill(&room, since, to, &mut count, &mut pages)
                        .await?;
                    recovered.append(&mut timeline.events);
                    timeline.events = recovered;
                }
                for event in timeline.events {
                    let Some(id) = event.event_id.clone() else {
                        continue;
                    };
                    if seen.insert(id.clone()) {
                        events.push((id, self.event(&room, event)));
                    }
                }
            }
        }
        let mut pending = Pending {
            discoveries,
            events,
            previous: since.map(str::to_owned),
            next: sync.next_batch,
        };
        let batch = pending.chunk();
        if !pending.discoveries.is_empty() {
            self.pending = Some(pending);
        }
        Ok(batch)
    }

    /// Fill the gap preceding a limited timeline, oldest first. Sync tokens
    /// are valid /messages tokens. Apply the same event filter on every page.
    /// No batch (and therefore no cursor) escapes until the whole gap is read.
    async fn backfill(
        &self,
        room: &str,
        since: &str,
        to: &str,
        count: &mut usize,
        pages: &mut usize,
    ) -> Result<Vec<RoomEvent>> {
        let mut from = since.to_owned();
        let mut tokens = BTreeSet::new();
        let mut events = Vec::new();
        while from != to {
            *pages += 1;
            if *pages > MAX_PAGES {
                return Err(MessengerError::SyncBudget { service: SERVICE });
            }
            if !tokens.insert(from.clone()) {
                return Err(MessengerError::SyncGap { service: SERVICE });
            }
            let mut url = self.url(&["rooms", room, "messages"])?;
            url.query_pairs_mut()
                .append_pair("from", &from)
                .append_pair("to", to)
                .append_pair("dir", "f")
                .append_pair("limit", "50")
                .append_pair(
                    "filter",
                    r#"{"types":["m.room.message","m.room.encrypted"]}"#,
                );
            let mut page: Page = self.request(HttpMethod::Get, url, None).await?;
            charge(count, page.chunk.len())?;
            events.append(&mut page.chunk);
            match page.end {
                Some(end) => from = end,
                // The Matrix API omits `end` when no further events remain.
                None => break,
            }
        }
        Ok(events)
    }

    fn event(&self, room: &str, event: RoomEvent) -> Event {
        if event
            .content
            .relation
            .as_ref()
            .and_then(|r| r.rel_type.as_deref())
            == Some("m.replace")
        {
            return Event::Ignored;
        }
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

    /// Require a successful full state read. A missing single state event
    /// (404) is ambiguous and must never authorize a plaintext reply.
    pub(crate) async fn check_room(&self, room: &str) -> Result<()> {
        let state: EncryptionState = self
            .request(HttpMethod::Get, self.url(&["rooms", room, "state"])?, None)
            .await?;
        if state.0 {
            Err(MessengerError::EncryptedRoom {
                chat: room.to_owned(),
            })
        } else {
            Ok(())
        }
    }

    pub(crate) async fn join(&self, room: &str) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                HttpMethod::Post,
                self.url(&["join", room])?,
                Some(serde_json::json!({})),
            )
            .await?;
        Ok(())
    }

    /// Send `text` to `room` as notices, split to stay within an event's
    /// size. The room's encryption is checked first, every time.
    pub(crate) async fn send(&self, room: &str, text: &str) -> Result<()> {
        for part in split(text, MAX_MESSAGE_UNITS) {
            self.check_room(room).await?;
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

fn charge(count: &mut usize, added: usize) -> Result<()> {
    *count = count.saturating_add(added);
    if *count > MAX_EVENTS {
        Err(MessengerError::SyncBudget { service: SERVICE })
    } else {
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
