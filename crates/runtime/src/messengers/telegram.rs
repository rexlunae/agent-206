//! Telegram Bot API adapter: long-polls `getUpdates` and answers with
//! `sendMessage`. The broker puts the bot token in the request path
//! ([`AuthScheme::TelegramBot`]); this side never sees it.
//!
//! Updates are confirmed by the next poll's offset, so a message is answered
//! at least once: if the gateway stops between answering and the next poll,
//! Telegram delivers that batch again after a restart.
use super::{Api, Inbound, MessengerError, Result};
use a206_broker::{AuthScheme, HttpMethod};
use serde::Deserialize;

const SERVICE: &str = "Telegram";
/// Seconds Telegram holds a poll open; well inside the broker's timeout.
const POLL_SECONDS: u32 = 25;
/// Telegram's limit for one message, in UTF-16 code units.
const MAX_MESSAGE_UNITS: usize = 4096;

pub(crate) struct Telegram {
    api: Api,
    /// The next update to ask for; earlier ones are confirmed.
    offset: Option<i64>,
}

#[derive(Deserialize)]
struct Envelope<T> {
    ok: bool,
    result: Option<T>,
}

#[derive(Deserialize)]
struct User {
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
    #[serde(default)]
    title: Option<String>,
}

impl Telegram {
    pub(crate) fn new(api: Api) -> Self {
        Self { api, offset: None }
    }

    async fn request<T: for<'de> Deserialize<'de>>(
        &self,
        method: HttpMethod,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<T> {
        let url = self.api.url(path)?;
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
    pub(crate) async fn connect(&mut self) -> Result<Option<String>> {
        let me: User = self.request(HttpMethod::Get, "getMe", None).await?;
        Ok(me.username.map(|name| format!("@{name}")).or(me.first_name))
    }

    /// Wait for the next batch of text messages.
    pub(crate) async fn receive(&mut self) -> Result<Vec<Inbound>> {
        let mut path =
            format!("getUpdates?timeout={POLL_SECONDS}&allowed_updates=%5B%22message%22%5D");
        if let Some(offset) = self.offset {
            path.push_str(&format!("&offset={offset}"));
        }
        let updates: Vec<Update> = self.request(HttpMethod::Get, &path, None).await?;
        if let Some(last) = updates.iter().map(|u| u.update_id).max() {
            self.offset = Some(last.saturating_add(1));
        }
        Ok(updates
            .into_iter()
            .filter_map(|update| {
                let message = update.message?;
                let text = message.text.filter(|t| !t.trim().is_empty())?;
                let title = message.chat.title.or_else(|| {
                    message
                        .from
                        .and_then(|u| u.username.map(|n| format!("@{n}")).or(u.first_name))
                });
                Some(Inbound {
                    chat: message.chat.id.to_string(),
                    title,
                    text,
                })
            })
            .collect())
    }

    /// Send `text` to `chat`, split to Telegram's message limit.
    pub(crate) async fn send(&self, chat: &str, text: &str) -> Result<()> {
        for part in split(text, MAX_MESSAGE_UNITS) {
            let _: serde_json::Value = self
                .request(
                    HttpMethod::Post,
                    "sendMessage",
                    Some(serde_json::json!({ "chat_id": chat, "text": part })),
                )
                .await?;
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::split;

    #[test]
    fn long_replies_split_at_newlines_within_the_limit() {
        let line = "x".repeat(7);
        let text = format!("{line}\n{line}\n{line}");
        let parts = split(&text, 10);
        assert_eq!(parts.concat(), text);
        assert_eq!(
            parts,
            [format!("{line}\n"), format!("{line}\n"), line.clone()]
        );
        // No newline: a hard split that never cuts a character, counting
        // UTF-16 units (an emoji is two).
        let wide = "😀".repeat(6);
        let parts = split(&wide, 5);
        assert_eq!(parts.concat(), wide);
        assert!(parts.iter().all(|p| p.encode_utf16().count() <= 5));
        assert!(split("", 5).is_empty());
    }
}
