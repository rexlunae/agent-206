//! Telegram Bot API adapter: long-polls `getUpdates` and answers with
//! `sendMessage`. The broker puts the bot token in the request path
//! ([`AuthScheme::TelegramBot`]); this side never sees it.
//!
//! Updates are confirmed by the next poll's offset. The caller keeps the
//! offset across reconnects, so while the gateway runs a message is handled
//! at most once; the offset is not persisted, so if the gateway stops before
//! the next poll, Telegram delivers that batch again after a restart.
use super::{Account, Api, Batch, Event, Inbound, MessengerError, Result, split};
use a206_broker::{AuthScheme, HttpMethod};
use serde::Deserialize;

const SERVICE: &str = "Telegram";
/// Seconds Telegram holds a poll open; well inside the broker's timeout.
const POLL_SECONDS: u32 = 25;
/// Telegram's limit for one message, in UTF-16 code units.
pub(super) const MAX_MESSAGE_UNITS: usize = 4096;

pub(crate) struct Telegram {
    api: Api,
}

#[derive(Deserialize)]
struct Envelope<T> {
    ok: bool,
    result: Option<T>,
}

#[derive(Deserialize)]
struct User {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    first_name: Option<String>,
}

#[derive(Deserialize)]
struct Update {
    update_id: i64,
    #[serde(default)]
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    chat: Chat,
    #[serde(default)]
    from: Option<User>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct Chat {
    id: i64,
    /// `private`, `group`, `supergroup` or `channel`.
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    title: Option<String>,
}

impl Telegram {
    pub(crate) fn new(api: Api) -> Self {
        Self { api }
    }

    async fn request<T: for<'de> Deserialize<'de>>(
        &self,
        method: HttpMethod,
        url: url::Url,
        body: Option<serde_json::Value>,
    ) -> Result<T> {
        let body = body
            .map(|body| serde_json::to_vec(&body))
            .transpose()
            .map_err(|source| MessengerError::Malformed {
                service: SERVICE,
                source,
            })?;
        let (status, bytes) = self
            .api
            .call(AuthScheme::TelegramBot, method, url, body)
            .await?;
        match status {
            200..=299 => {}
            401 | 404 => return Err(MessengerError::Unauthorized { service: SERVICE }),
            409 => return Err(MessengerError::PollingConflict { service: SERVICE }),
            status => {
                return Err(MessengerError::Status {
                    service: SERVICE,
                    status,
                });
            }
        }
        let envelope: Envelope<T> =
            serde_json::from_slice(&bytes).map_err(|source| MessengerError::Malformed {
                service: SERVICE,
                source,
            })?;
        match envelope {
            Envelope {
                ok: true,
                result: Some(result),
            } => Ok(result),
            _ => Err(MessengerError::Refused { service: SERVICE }),
        }
    }

    /// Check the token and name the bot account.
    pub(crate) async fn connect(&mut self) -> Result<Account> {
        let me: User = self
            .request(HttpMethod::Get, self.api.url("getMe")?, None)
            .await?;
        Ok(Account {
            id: me
                .id
                .ok_or(MessengerError::NoAccountId { service: SERVICE })?
                .to_string(),
            name: me.username.map(|name| format!("@{name}")).or(me.first_name),
        })
    }

    /// Wait for the next batch of updates after `offset`, which confirms
    /// every earlier one. The batch's cursor is the offset past its last
    /// update.
    pub(crate) async fn receive(&mut self, offset: Option<&str>) -> Result<Batch> {
        let mut url = self.api.url("getUpdates")?;
        url.query_pairs_mut()
            .append_pair("timeout", &POLL_SECONDS.to_string())
            .append_pair("allowed_updates", r#"["message"]"#);
        if let Some(offset) = offset {
            url.query_pairs_mut().append_pair("offset", offset);
        }
        let mut updates: Vec<Update> = self.request(HttpMethod::Get, url, None).await?;
        updates.sort_by_key(|u| u.update_id);
        let next = updates
            .last()
            .map(|last| last.update_id.saturating_add(1).to_string())
            .or_else(|| offset.map(str::to_owned));
        let events = updates
            .into_iter()
            .map(|update| {
                let event = update
                    .message
                    .and_then(inbound)
                    .map_or(Event::Ignored, Event::Message);
                (update.update_id.to_string(), event)
            })
            .collect();
        Ok(Batch {
            events,
            next,
            complete: true,
        })
    }

    /// Send `text` to `chat`, split to Telegram's message limit.
    pub(crate) async fn send(&self, chat: &str, text: &str) -> Result<()> {
        for part in split(text, MAX_MESSAGE_UNITS) {
            let _: serde_json::Value = self
                .request(
                    HttpMethod::Post,
                    self.api.url("sendMessage")?,
                    Some(serde_json::json!({ "chat_id": chat, "text": part })),
                )
                .await?;
        }
        Ok(())
    }
}

fn inbound(message: Message) -> Option<Inbound> {
    let text = message.text.filter(|t| !t.trim().is_empty())?;
    let sender = message
        .from
        .as_ref()
        .and_then(|u| u.id)
        .map(|id| id.to_string());
    let title = message.chat.title.or_else(|| {
        message
            .from
            .and_then(|u| u.username.map(|n| format!("@{n}")).or(u.first_name))
    });
    Some(Inbound {
        chat: message.chat.id.to_string(),
        sender,
        // Anything not known to be private is treated as a group.
        group: message.chat.kind.as_deref() != Some("private"),
        title,
        text,
        encrypted: false,
    })
}
