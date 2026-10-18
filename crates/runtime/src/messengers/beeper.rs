//! Beeper Desktop API adapter. Beeper Desktop runs on the user's computer,
//! connects to the chat networks the user added to it (WhatsApp, iMessage,
//! Google Chat and others) and serves a local HTTP API. The broker attaches
//! the user's Desktop API token as a bearer token for that origin; this side
//! never sees it.
//!
//! Replies are sent as the user, from their own accounts on those networks,
//! the same way as a message typed in Beeper. Only allowed chats are read:
//! the chat list says which chats have new activity, and only allowed ones
//! have their messages fetched. Other active chats are reported as refused,
//! by id and title, so the user can allow one.
//!
//! Beeper's API has no long poll, so the chat list is polled. A session
//! starts by marking where each allowed chat's messages end, so history is
//! never answered. The cursors are kept across reconnects while the gateway
//! runs, but not across restarts: messages that arrive while the gateway is
//! not running are not answered.
use super::{Account, Api, Batch, Event, Inbound, MessengerError, Result};
use a206_broker::{AuthScheme, HttpMethod};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

const SERVICE: &str = "Beeper";
/// How long to wait before looking at the chat list again when nothing
/// changed.
const POLL: Duration = if cfg!(test) {
    Duration::from_millis(20)
} else {
    Duration::from_secs(2)
};
/// Changed chats one look at the chat list reads. When more changed, every
/// allowed chat is checked directly, so a burst of activity elsewhere cannot
/// hide one.
const CHAT_PAGE: &str = "100";
/// Pages of one chat's new messages read per poll; the rest wait for the
/// next.
const MAX_MESSAGE_PAGES: usize = 8;
/// Parts a long reply is split into, in UTF-16 code units.
const MAX_MESSAGE: usize = 4000;
const MAX_PARTS: usize = 8;

pub(crate) struct Beeper {
    api: Api,
    allowed: BTreeSet<String>,
    /// Where reading stands once every batch handed out so far is handled.
    committed: Option<State>,
    /// Where reading stands once the batch last handed out is handled. The
    /// gateway confirms it by asking for the next batch with its generation;
    /// any other cursor means it was not, and reading resumes from
    /// `committed`.
    pending: Option<State>,
    generations: u64,
}

#[derive(Clone, Debug, PartialEq)]
struct State {
    generation: u64,
    /// The chat list's cursor: chats with activity after it have changed.
    chats: Option<String>,
    /// Each allowed chat's message cursor: messages after it are new.
    messages: BTreeMap<String, String>,
    /// Allowed chats with more new messages than one poll reads.
    behind: BTreeSet<String>,
}

#[derive(Deserialize)]
struct AccountEntry {
    #[serde(rename = "accountID")]
    account_id: String,
    #[serde(default)]
    network: Option<String>,
}

#[derive(Deserialize)]
struct Page<T> {
    #[serde(default = "Vec::new")]
    items: Vec<T>,
    #[serde(default, rename = "hasMore")]
    has_more: bool,
    #[serde(default, rename = "newestCursor")]
    newest_cursor: Option<String>,
}

#[derive(Clone, Deserialize)]
struct Chat {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    network: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    /// The chat's latest message, read only to see who wrote it.
    #[serde(default)]
    preview: Option<serde_json::Value>,
}
impl Chat {
    /// Only a chat Beeper calls single has one other member.
    fn group(&self) -> bool {
        self.kind.as_deref() != Some("single")
    }
    /// The chat's title with its network, for the refused-chat list.
    fn label(&self) -> Option<String> {
        match (&self.network, &self.title) {
            (Some(network), Some(title)) => Some(format!("{network}: {title}")),
            (None, title) => title.clone(),
            (network, None) => network.clone(),
        }
    }
}

#[derive(Clone, Deserialize)]
struct Message {
    #[serde(default)]
    id: String,
    #[serde(default, rename = "senderID")]
    sender_id: Option<String>,
    #[serde(default, rename = "senderName")]
    sender_name: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default, rename = "isSender")]
    is_sender: Option<bool>,
    #[serde(default, rename = "isDeleted")]
    is_deleted: Option<bool>,
    #[serde(default, rename = "isHidden")]
    is_hidden: Option<bool>,
}

impl Beeper {
    pub(crate) fn new(api: Api, allowed: BTreeSet<String>) -> Self {
        Self {
            api,
            allowed,
            committed: None,
            pending: None,
            generations: 0,
        }
    }

    fn url(&self, path: &[&str]) -> Result<url::Url> {
        let segments: Vec<&str> = ["v1"].iter().chain(path).copied().collect();
        self.api.url_segments(&segments)
    }

    /// The parsed body of a success, or `None` for a chat or message Beeper
    /// does not have (404).
    async fn fetch<T: for<'de> Deserialize<'de>>(
        &self,
        method: HttpMethod,
        url: url::Url,
        body: Option<serde_json::Value>,
    ) -> Result<Option<T>> {
        let body = body
            .map(|body| serde_json::to_vec(&body))
            .transpose()
            .map_err(|source| MessengerError::Malformed {
                service: SERVICE,
                source,
            })?;
        let (status, bytes) = self.api.call(AuthScheme::Bearer, method, url, body).await?;
        match status {
            200..=299 => serde_json::from_slice(&bytes).map(Some).map_err(|source| {
                MessengerError::Malformed {
                    service: SERVICE,
                    source,
                }
            }),
            404 => Ok(None),
            401 | 403 => Err(MessengerError::Unauthorized { service: SERVICE }),
            status => Err(MessengerError::Status {
                service: SERVICE,
                status,
            }),
        }
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, url: url::Url) -> Result<T> {
        self.fetch(HttpMethod::Get, url, None)
            .await?
            .ok_or(MessengerError::Status {
                service: SERVICE,
                status: 404,
            })
    }

    /// Check the token and name the chat accounts it reaches.
    pub(crate) async fn connect(&mut self) -> Result<Account> {
        let accounts: Vec<AccountEntry> = self.get(self.url(&["accounts"])?).await?;
        let ids: BTreeSet<&str> = accounts.iter().map(|a| a.account_id.as_str()).collect();
        if ids.is_empty() {
            return Err(MessengerError::BeeperNoAccounts);
        }
        let networks: BTreeSet<&str> = accounts
            .iter()
            .filter_map(|a| a.network.as_deref())
            .collect();
        let name = if networks.is_empty() {
            SERVICE.to_owned()
        } else {
            format!(
                "{SERVICE}: {}",
                networks.into_iter().collect::<Vec<_>>().join(", ")
            )
        };
        Ok(Account {
            id: ids.into_iter().collect::<Vec<_>>().join(","),
            name: Some(name),
        })
    }

    fn generation(&mut self) -> u64 {
        self.generations += 1;
        self.generations
    }

    /// Wait for new messages in allowed chats, and activity elsewhere, after
    /// the state `since` names. Without one (or with one this adapter did not
    /// hand out), this marks where answering starts and returns at once.
    pub(crate) async fn receive(&mut self, since: Option<&str>) -> Result<Batch> {
        let since = since.and_then(|since| since.parse::<u64>().ok());
        if let Some(pending) = self.pending.take()
            && Some(pending.generation) == since
        {
            self.committed = Some(pending);
        }
        let current = match self.committed.clone() {
            Some(state) if Some(state.generation) == since => state,
            _ => {
                let state = self.start().await?;
                let next = Some(state.generation.to_string());
                self.committed = Some(state);
                return Ok(Batch {
                    events: Vec::new(),
                    complete: true,
                    next,
                });
            }
        };
        let (mut state, events, more) = self.poll(&current).await?;
        if events.is_empty() {
            // Nothing to answer: the new position needs no confirmation. The
            // empty batch hands control back, so the gateway checks which
            // accounts the token reaches before every look.
            let next = Some(state.generation.to_string());
            self.committed = Some(state);
            if !more {
                tokio::time::sleep(POLL).await;
            }
            return Ok(Batch {
                events,
                complete: true,
                next,
            });
        }
        state.generation = self.generation();
        let next = Some(state.generation.to_string());
        self.pending = Some(state);
        Ok(Batch {
            events,
            complete: true,
            next,
        })
    }

    /// The chat list's newest cursor, and where each allowed chat's messages
    /// end now.
    async fn start(&mut self) -> Result<State> {
        let mut url = self.url(&["chats"])?;
        url.query_pairs_mut().append_pair("limit", "1");
        let first: Page<Chat> = self.get(url).await?;
        let mut state = State {
            generation: self.generation(),
            chats: first.newest_cursor,
            messages: BTreeMap::new(),
            behind: BTreeSet::new(),
        };
        for chat in self.allowed.clone() {
            if let Some(cursor) = self.newest(&chat).await? {
                state.messages.insert(chat, cursor);
            }
        }
        Ok(state)
    }

    /// Where `chat`'s messages end, if Beeper has the chat.
    async fn newest(&self, chat: &str) -> Result<Option<String>> {
        Ok(self.latest(chat).await?.and_then(|page| page.newest_cursor))
    }

    /// `chat`'s most recent messages, if Beeper has the chat.
    async fn latest(&self, chat: &str) -> Result<Option<Page<Message>>> {
        self.fetch(
            HttpMethod::Get,
            self.url(&["chats", chat, "messages"])?,
            None,
        )
        .await
    }

    /// One look at the chat list from `current`: the state after it, the
    /// events it found, and whether more is waiting to be read at once.
    async fn poll(&self, current: &State) -> Result<(State, Vec<(String, Event)>, bool)> {
        let mut state = current.clone();
        let mut changed = BTreeMap::new();
        let mut url = self.url(&["chats"])?;
        url.query_pairs_mut().append_pair("limit", CHAT_PAGE);
        if let Some(cursor) = &state.chats {
            url.query_pairs_mut()
                .append_pair("cursor", cursor)
                .append_pair("direction", "after");
        }
        let page: Page<Chat> = self.get(url).await?;
        // Without a cursor this was the most recent page of every chat, not
        // a list of changes. With one, a full page may have left changed
        // chats out, whichever end of the changes it holds.
        let overflow = state.chats.is_some() && page.has_more;
        if let Some(newest) = page.newest_cursor {
            state.chats = Some(newest);
        }
        for chat in page.items {
            changed.insert(chat.id.clone(), chat);
        }
        let mut events = Vec::new();
        let mut targets = std::mem::take(&mut state.behind);
        for (id, chat) in &changed {
            if self.allowed.contains(id) {
                targets.insert(id.clone());
            } else if !chat.preview.as_ref().is_some_and(|preview| {
                preview.get("isSender") == Some(&serde_json::Value::Bool(true))
            }) {
                // Activity the user caused needs no mention; anyone else's
                // may be someone trying to reach the agent.
                events.push((
                    format!("chat {id}"),
                    Event::Discovered {
                        chat: id.clone(),
                        title: chat.label(),
                    },
                ));
            }
        }
        if overflow {
            targets.extend(self.allowed.iter().cloned());
        }
        for chat in targets {
            let messages = match state.messages.get(&chat).cloned() {
                Some(cursor) => {
                    let (cursor, messages, behind) = self.after(&chat, cursor).await?;
                    state.messages.insert(chat.clone(), cursor);
                    if behind {
                        state.behind.insert(chat.clone());
                    }
                    messages
                }
                // An allowed chat that had no messages when the session
                // started (Beeper did not have it, or it was empty): all it
                // holds now arrived since.
                None => match self.latest(&chat).await? {
                    Some(page) => {
                        if let Some(cursor) = page.newest_cursor {
                            state.messages.insert(chat.clone(), cursor);
                        }
                        page.items
                    }
                    None => continue,
                },
            };
            if messages.is_empty() {
                continue;
            }
            let info = match changed.get(&chat) {
                Some(info) => Some(info.clone()),
                None => {
                    self.fetch(HttpMethod::Get, self.url(&["chats", &chat])?, None)
                        .await?
                }
            };
            for message in messages {
                // Without an id a message has no key to answer it once by.
                if message.id.is_empty() {
                    continue;
                }
                let key = format!("{chat}:{}", message.id);
                events.push((key, event(&chat, info.as_ref(), message)));
            }
        }
        let more = overflow || !state.behind.is_empty();
        Ok((state, events, more))
    }

    /// `chat`'s messages after `cursor`, oldest first: the cursor after them,
    /// the messages, and whether more remain past this poll's pages.
    async fn after(&self, chat: &str, mut cursor: String) -> Result<(String, Vec<Message>, bool)> {
        let mut messages = Vec::new();
        for _ in 0..MAX_MESSAGE_PAGES {
            let mut url = self.url(&["chats", chat, "messages"])?;
            url.query_pairs_mut()
                .append_pair("cursor", &cursor)
                .append_pair("direction", "after");
            let Some(page): Option<Page<Message>> = self.fetch(HttpMethod::Get, url, None).await?
            else {
                // The chat is gone from Beeper; nothing more will arrive.
                return Ok((cursor, messages, false));
            };
            let read = page.items.len();
            messages.extend(page.items);
            match page.newest_cursor {
                Some(newest) if newest != cursor => {
                    cursor = newest;
                    if !page.has_more {
                        return Ok((cursor, messages, false));
                    }
                }
                _ if read == 0 => return Ok((cursor, messages, false)),
                // New messages without a cursor past them would be read, and
                // answered, again on every poll.
                _ => return Err(MessengerError::SyncGap { service: SERVICE }),
            }
        }
        Ok((cursor, messages, true))
    }

    /// Send `text` to `chat` as the user, in bounded parts.
    pub(crate) async fn send(&self, chat: &str, text: &str) -> Result<()> {
        for part in super::bounded_parts(text, MAX_MESSAGE, MAX_PARTS) {
            let sent: Option<serde_json::Value> = self
                .fetch(
                    HttpMethod::Post,
                    self.url(&["chats", chat, "messages"])?,
                    Some(serde_json::json!({ "text": part })),
                )
                .await?;
            // Beeper names the pending message once it accepted the send.
            let accepted = sent
                .as_ref()
                .and_then(|sent| sent.get("pendingMessageID"))
                .is_some_and(serde_json::Value::is_string);
            if !accepted {
                return Err(MessengerError::Refused { service: SERVICE });
            }
        }
        Ok(())
    }
}

/// A message in `chat` as the supervisor sees it. Only other people's text
/// is answered: the user's own messages (which include the agent's replies),
/// notices from bots and network services, deleted, hidden and media
/// messages are not.
fn event(chat: &str, info: Option<&Chat>, message: Message) -> Event {
    if message.is_sender == Some(true)
        || message.is_deleted == Some(true)
        || message.is_hidden == Some(true)
        || !matches!(message.kind.as_deref(), None | Some("TEXT"))
    {
        return Event::Ignored;
    }
    let (Some(text), Some(sender)) = (
        message.text.filter(|text| !text.trim().is_empty()),
        message.sender_id,
    ) else {
        return Event::Ignored;
    };
    // A chat whose kind is unknown counts as a group, which answers only
    // listed senders.
    let group = info.is_none_or(Chat::group);
    let title = if group {
        info.and_then(Chat::label)
    } else {
        message.sender_name.or_else(|| info.and_then(Chat::label))
    };
    Event::Message(Inbound {
        chat: chat.to_owned(),
        sender: Some(sender),
        group,
        title,
        text,
        encrypted: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    fn message(value: serde_json::Value) -> Message {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn only_other_peoples_text_is_answered() {
        let (chat, sender, text) = (id(), id(), id());
        let single = Chat {
            id: chat.clone(),
            title: Some(id()),
            network: Some(id()),
            kind: Some("single".to_owned()),
            preview: None,
        };
        let base = serde_json::json!({ "id": id(), "senderID": sender, "senderName": "Ana", "text": text, "type": "TEXT" });
        let Event::Message(direct) = event(&chat, Some(&single), message(base.clone())) else {
            panic!("a direct text message is answered");
        };
        assert!(!direct.group);
        assert_eq!(direct.sender.as_deref(), Some(sender.as_str()));
        assert_eq!(direct.title.as_deref(), Some("Ana"));
        assert_eq!(direct.text, text);
        for (field, value) in [
            ("isSender", serde_json::json!(true)),
            ("isDeleted", serde_json::json!(true)),
            ("isHidden", serde_json::json!(true)),
            ("type", serde_json::json!("NOTICE")),
            ("type", serde_json::json!("IMAGE")),
            ("text", serde_json::json!("  ")),
            ("senderID", serde_json::Value::Null),
        ] {
            let mut changed = base.clone();
            changed[field] = value;
            assert_eq!(
                event(&chat, Some(&single), message(changed)),
                Event::Ignored,
                "{field}"
            );
        }
        // Group and unknown chats answer only listed senders.
        let group = Chat {
            kind: Some("group".to_owned()),
            ..single.clone()
        };
        for info in [Some(&group), None] {
            let Event::Message(inbound) = event(&chat, info, message(base.clone())) else {
                panic!("a group text message is passed on");
            };
            assert!(inbound.group);
        }
    }
}
