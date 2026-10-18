//! Webhook messengers: incoming webhooks of chat services (Slack, Discord,
//! Google Chat, Teams) or any JSON endpoint. They only send. The whole
//! webhook URL is the vault credential, because its path and query carry the
//! key; the broker sends to it ([`AuthScheme::WebhookUrl`]) and this side
//! names only the URL's origin.
use super::{Api, MessengerError, Result};
use a206_broker::{AuthScheme, HttpMethod};
use a206_core::{BrokerPairing, MessengerConfig, WebhookFormat};
use serde_json::{Value, json};
use std::path::PathBuf;

/// Most parts one message is sent in; a longer message is refused whole
/// rather than cut.
pub const MAX_PARTS: usize = 8;

/// Longest part, in UTF-16 units, each format takes: Discord's 2,000
/// character content limit, and for the others the 4,000 characters Slack
/// recommends, which is also within Google Chat's and Teams' limits.
pub fn part_limit(format: WebhookFormat) -> usize {
    match format {
        WebhookFormat::Discord => 2000,
        WebhookFormat::Text | WebhookFormat::Teams => 4000,
    }
}

/// `text` as the parts it is sent in, or why it cannot be sent.
pub fn parts(format: WebhookFormat, text: &str) -> Result<Vec<String>> {
    let limit = part_limit(format);
    let parts: Vec<String> = super::split(text, limit)
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .map(str::to_owned)
        .collect();
    if parts.is_empty() {
        return Err(MessengerError::EmptyMessage);
    }
    if parts.len() > MAX_PARTS {
        return Err(MessengerError::MessageTooLong {
            most: limit * MAX_PARTS,
        });
    }
    Ok(parts)
}

/// The request body for one part.
fn body(format: WebhookFormat, text: &str) -> Value {
    match format {
        WebhookFormat::Text => json!({ "text": text }),
        // Mentions in the text never ping anyone.
        WebhookFormat::Discord => json!({ "content": text, "allowed_mentions": { "parse": [] } }),
        WebhookFormat::Teams => json!({
            "type": "message",
            "attachments": [{
                "contentType": "application/vnd.microsoft.card.adaptive",
                "content": {
                    "type": "AdaptiveCard",
                    "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
                    "version": "1.4",
                    "body": [{ "type": "TextBlock", "text": text, "wrap": true }]
                }
            }]
        }),
    }
}

/// Send `text` to the webhook `config` names, in order; the number of parts
/// sent. A part that fails stops the rest, and the error says how many were
/// already delivered.
pub async fn send(
    root: PathBuf,
    pairing: BrokerPairing,
    config: &MessengerConfig,
    text: &str,
) -> Result<usize> {
    let parts = parts(config.format, text)?;
    let api = Api::new(root, pairing, config)?;
    let total = parts.len();
    for (sent, part) in parts.iter().enumerate() {
        let delivered = async {
            let body = serde_json::to_vec(&body(config.format, part)).map_err(|source| {
                MessengerError::Malformed {
                    service: api.service,
                    source,
                }
            })?;
            let (status, _) = api
                .call(
                    AuthScheme::WebhookUrl,
                    HttpMethod::Post,
                    api.base.clone(),
                    Some(body),
                )
                .await?;
            if !(200..300).contains(&status) {
                return Err(match status {
                    401 | 403 | 404 => MessengerError::Unauthorized {
                        service: api.service,
                    },
                    status => MessengerError::Status {
                        service: api.service,
                        status,
                    },
                });
            }
            Ok(())
        }
        .await;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn random() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    #[test]
    fn each_format_carries_the_text_where_its_service_reads_it() {
        let text = random();
        assert_eq!(body(WebhookFormat::Text, &text)["text"], text);
        let discord = body(WebhookFormat::Discord, &text);
        assert_eq!(discord["content"], text);
        assert_eq!(discord["allowed_mentions"]["parse"], json!([]));
        let teams = body(WebhookFormat::Teams, &text);
        assert_eq!(teams["attachments"][0]["content"]["body"][0]["text"], text);
    }

    #[test]
    fn long_messages_split_and_too_long_or_blank_ones_are_refused_whole() {
        for format in WebhookFormat::ALL {
            let limit = part_limit(format);
            let fits = "x".repeat(limit * MAX_PARTS);
            let sent = parts(format, &fits).unwrap();
            assert_eq!(sent.len(), MAX_PARTS);
            assert_eq!(sent.concat(), fits);
            assert!(matches!(
                parts(format, &format!("{fits}x")),
                Err(MessengerError::MessageTooLong { most }) if most == limit * MAX_PARTS
            ));
            assert!(matches!(
                parts(format, " \n "),
                Err(MessengerError::EmptyMessage)
            ));
        }
    }
}
