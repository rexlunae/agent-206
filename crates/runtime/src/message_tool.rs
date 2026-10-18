//! Sending messages out of Agent 206 for the native run engine: a
//! [`ToolHost`] whose `send_message` posts to the user's webhook messengers,
//! as RustyClaw's `message` tool does.
//!
//! A message leaves the app, so the tool is denied until the user permits it
//! on the conversation, and each call then still waits for the user's
//! approval of that exact destination and text. A model names a webhook only
//! by the name the user gave it: the URL stays in the vault, and only
//! enabled webhooks the user has not disconnected are offered.
use crate::{
    gateway::Outbox,
    tools::{ToolHost, ToolListing, ToolOutput, ToolRequest, ToolScope, ToolSpec},
};
use a206_core::{ErrorKind, ErrorReport, ToolPermission};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};

/// Sends a message to one of the user's webhooks.
pub const SEND: &str = "send_message";

/// Most bytes the destination list takes in the schema. The catalog drops a
/// tool whose schema passes 32 KiB, and a user may allow hundreds of long
/// chat ids; beyond this the destinations are not listed, and the gateway
/// still checks each one it is given.
const MAX_LISTED: usize = 24 * 1024;

/// Why a `send_message` call was refused before anything was sent.
#[derive(Debug, thiserror::Error)]
pub enum MessageToolError {
    #[error("`{0}` is required")]
    Missing(&'static str),
    #[error("`{0}` has the wrong type")]
    Type(&'static str),
    #[error("unknown argument `{0}`")]
    Unknown(String),
    #[error("unknown message tool `{0}`")]
    UnknownTool(String),
    #[error("the user did not approve this message")]
    NotApproved,
}
impl crate::error::Classified for MessageToolError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::UnknownTool(_) => ErrorKind::NotFound,
            Self::NotApproved => ErrorKind::PermissionDenied,
            _ => ErrorKind::InvalidRequest,
        })
    }
}
fn fail(error: MessageToolError) -> ErrorReport {
    crate::error::report(&error)
}

/// Offers `send_message` while the user has a webhook to send to.
pub struct MessageHost {
    outbox: Outbox,
}
impl MessageHost {
    pub fn new(outbox: Outbox) -> Self {
        Self { outbox }
    }

    fn spec(destinations: Vec<String>) -> ToolSpec {
        let listed =
            serde_json::to_vec(&destinations).map_or(usize::MAX, |list| list.len()) <= MAX_LISTED;
        let mut description = String::from(
            "Send a message to one of the user's webhooks, such as a team chat channel, or to a chat the user allowed on one of their messengers (named `messenger/chat`). It is posted as it is, outside this conversation, and the user approves each call. Long messages are sent in a few parts, leaving out any part that would hold only whitespace; a message too long for the destination is refused whole.",
        );
        let mut destination = json!({"type": "string", "description": "A webhook by the name the user gave it, or `messenger/chat` for an allowed chat of a messenger."});
        if listed {
            destination["enum"] = json!(destinations);
        } else {
            description.push_str(" The user allowed too many destinations to list here; ask them which webhook or chat they mean.");
        }
        ToolSpec {
            name: SEND.into(),
            description,
            parameters: json!({
                "type": "object",
                "properties": {
                    "destination": destination,
                    "text": {"type": "string", "description": "The message, as plain text."}
                },
                "required": ["destination", "text"],
                "additionalProperties": false
            }),
            capabilities: Default::default(),
            permission: ToolPermission::Ask,
            whole_output: false,
        }
    }

    async fn run(&self, request: &ToolRequest) -> Result<Value, ErrorReport> {
        if request.name != SEND {
            return Err(fail(MessageToolError::UnknownTool(request.name.clone())));
        }
        // The gateway asks the user first; never send on a call it did not.
        if !request.approved {
            return Err(fail(MessageToolError::NotApproved));
        }
        let args = &request.arguments;
        if let Some(key) = args
            .keys()
            .find(|key| !matches!(key.as_str(), "destination" | "text"))
        {
            return Err(fail(MessageToolError::Unknown(key.clone())));
        }
        let destination = text(args, "destination")?;
        let message = text(args, "text")?;
        let parts = self
            .outbox
            .send(destination, message)
            .await
            .map_err(|error| crate::error::report(&error))?;
        Ok(json!({ "sent_to": destination, "parts": parts }))
    }
}

fn text<'a>(args: &'a Map<String, Value>, name: &'static str) -> Result<&'a str, ErrorReport> {
    args.get(name)
        .filter(|value| !value.is_null())
        .ok_or_else(|| fail(MessageToolError::Missing(name)))?
        .as_str()
        .ok_or_else(|| fail(MessageToolError::Type(name)))
}

impl ToolHost for MessageHost {
    fn list<'a>(&'a self, _scope: &'a ToolScope) -> BoxFuture<'a, ToolListing> {
        Box::pin(async move {
            let destinations = self.outbox.destinations();
            ToolListing {
                tools: if destinations.is_empty() {
                    vec![]
                } else {
                    vec![Self::spec(destinations)]
                },
                unavailable: vec![],
            }
        })
    }
    fn call<'a>(
        &'a self,
        _scope: &'a ToolScope,
        request: ToolRequest,
    ) -> BoxFuture<'a, Result<ToolOutput, ErrorReport>> {
        Box::pin(async move {
            self.run(&request).await.map(|value| ToolOutput {
                text: value.to_string(),
                is_error: false,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destinations_are_listed_unless_the_list_would_crowd_out_the_tool() {
        let name = || uuid::Uuid::new_v4().simple().to_string();
        let few: Vec<String> = (0..3).map(|_| name()).collect();
        let spec = MessageHost::spec(few.clone());
        assert_eq!(
            spec.parameters["properties"]["destination"]["enum"],
            json!(few)
        );
        // 256 chats of 255-byte ids, the most one messenger may allow.
        let many: Vec<String> = (0..256)
            .map(|_| format!("{}/{}", name(), "x".repeat(255)))
            .collect();
        let spec = MessageHost::spec(many);
        let destination = &spec.parameters["properties"]["destination"];
        assert!(destination.get("enum").is_none());
        assert_eq!(destination["type"], "string");
        let schema = serde_json::to_vec(&spec.parameters).unwrap().len();
        assert!(schema < 32 * 1024, "{schema}");
        assert!(spec.description.len() <= 4096);
    }
}
