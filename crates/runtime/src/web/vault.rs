//! Bounded ciphertext relay. Each authenticated browser channel owns one
//! role-limited broker socket. This process never handles vault plaintext.
use super::*;
use a206_broker::{Client, Reply as BrokerReply, Request as BrokerRequest, Role};
use a206_vault_channel::{RelayReply, RelayRequest};
use std::collections::HashMap;
use tokio::sync::{mpsc, oneshot};
const IDLE: std::time::Duration = std::time::Duration::from_secs(300);
const LIMIT: usize = 32;
type Job = (
    Vec<u8>,
    oneshot::Sender<Result<Vec<u8>, a206_broker::ClientError>>,
);
struct Channel {
    owner: String,
    sender: mpsc::Sender<Job>,
}
#[derive(Default)]
pub(super) struct Relay {
    channels: Mutex<HashMap<String, Channel>>,
}
impl Relay {
    fn table(&self) -> std::sync::MutexGuard<'_, HashMap<String, Channel>> {
        self.channels.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
fn unavailable(message: &str) -> RelayReply {
    RelayReply::Error(ErrorReport::new(a206_core::ErrorKind::Unavailable, message))
}
fn owner(headers: &HeaderMap) -> String {
    use sha2::Digest;
    // Never retain or log the browser's bearer as a relay identifier.
    let value = headers
        .get(header::AUTHORIZATION)
        .map_or(&[][..], |value| value.as_bytes());
    format!("{:x}", sha2::Sha256::digest(value))
}
pub(super) async fn handle(State(state): State<Web>, headers: HeaderMap, bytes: Bytes) -> Response {
    if !authorized(&state, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut decoder = Decoder::default();
    let Ok(mut requests) = decoder.feed::<Request<RelayRequest>>(&bytes) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if requests.len() != 1 || decoder.finish().is_err() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let request = requests.remove(0);
    let id = request.id;
    let reply = if !matches!(request.body, RelayRequest::Close { .. })
        && !matches!(state.status().active, Some(GatewayTarget::Local))
    {
        unavailable(
            "Choose a local gateway first. Remote vault setup over SSH is not available yet.",
        )
    } else {
        exchange(&state, owner(&headers), request.body)
            .await
            .unwrap_or_else(|error| {
                RelayReply::Error(crate::error::correlated(
                    "vault ciphertext relay",
                    id,
                    &error,
                ))
            })
    };
    respond(StatusCode::OK, id, &reply)
}
async fn connect(root: &std::path::Path) -> Result<Client, crate::vault::VaultError> {
    // Starts the broker if needed. No password or secret-bearing request is sent.
    let user = crate::vault::connect(root).await?;
    if user.version() < a206_broker::SETUP_CHANNEL_VERSION {
        return Err(crate::vault::VaultError::ChannelUnsupported);
    }
    drop(user);
    Ok(Client::connect(root, Role::Relay).await?)
}

async fn exchange(
    state: &Web,
    owner: String,
    request: RelayRequest,
) -> Result<RelayReply, crate::vault::VaultError> {
    use crate::vault::VaultError;
    match request {
        RelayRequest::Key => {
            let mut client = connect(&state.root).await?;
            match client.call(BrokerRequest::ChannelKey).await? {
                BrokerReply::ChannelKey(key) => Ok(RelayReply::Key(key)),
                _ => Err(VaultError::UnexpectedReply),
            }
        }
        RelayRequest::Open { first } => {
            if first.len() > a206_vault_channel::MAX_MESSAGE {
                return Ok(unavailable("Vault handshake is too large."));
            }
            let mut client = connect(&state.root).await?;
            let BrokerReply::ChannelOpened(response) =
                client.call(BrokerRequest::OpenChannel { first }).await?
            else {
                return Err(VaultError::UnexpectedReply);
            };
            let (sender, mut receiver) = mpsc::channel::<Job>(1);
            let session = uuid::Uuid::new_v4().to_string();
            {
                let mut channels = state.vault.table();
                channels.retain(|_, channel| !channel.sender.is_closed());
                if channels.len() >= LIMIT {
                    return Ok(unavailable(
                        "Too many vault sessions. Close unused security panels and try again.",
                    ));
                }
                channels.insert(session.clone(), Channel { owner, sender });
            }
            // Detached deliberately: the worker owns the socket, ends on close,
            // idle timeout or transport failure, and delivers each result to its caller.
            tokio::spawn(async move {
                while let Ok(Some((bytes, reply))) =
                    tokio::time::timeout(IDLE, receiver.recv()).await
                {
                    let result = match tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        client.call(BrokerRequest::ChannelMessage { bytes }),
                    )
                    .await
                    {
                        Ok(Ok(BrokerReply::ChannelMessage(bytes))) => Ok(bytes),
                        Ok(Err(error)) => Err(error),
                        Ok(Ok(_)) => Err(a206_broker::ClientError::Closed),
                        Err(error) => Err(a206_broker::ClientError::Io(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            error,
                        ))),
                    };
                    let stop = result.is_err();
                    // A disappeared browser must not leave a connection with an
                    // unknown nonce or an authenticated step-up alive.
                    if reply.send(result).is_err() || stop {
                        break;
                    }
                }
            });
            Ok(RelayReply::Opened { session, response })
        }
        RelayRequest::Exchange { session, bytes } => {
            if bytes.len() > a206_vault_channel::MAX_MESSAGE {
                return Ok(unavailable("Vault message is too large."));
            }
            let sender = state
                .vault
                .table()
                .get(&session)
                .filter(|channel| channel.owner == owner)
                .map(|channel| channel.sender.clone());
            let Some(sender) = sender else {
                return Ok(unavailable(
                    "Vault session ended. Reconnect and verify the broker key.",
                ));
            };
            let (reply, receiver) = oneshot::channel();
            if sender.try_send((bytes, reply)).is_err() {
                return Ok(unavailable(
                    "Vault channel is busy or ended. Wait for the current operation, then reconnect if needed.",
                ));
            }
            let result = receiver.await.map_err(|_| VaultError::Worker(None))?;
            result.map(RelayReply::Message).map_err(VaultError::from)
        }
        RelayRequest::Close { session } => {
            let mut channels = state.vault.table();
            if channels
                .get(&session)
                .is_some_and(|channel| channel.owner == owner)
            {
                channels.remove(&session);
            }
            Ok(RelayReply::Closed)
        }
    }
}

#[cfg(test)]
mod compatibility_tests {
    use super::*;
    use crate::transport::{read_frame, write_frame};
    use a206_broker::{BrokerError, InvalidRequest};
    use a206_protocol::{Request as Frame, Response};

    #[tokio::test]
    async fn messenger_versions_cannot_open_a_setup_relay() {
        for version in [8, 9] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join("vault")).unwrap();
            let listener =
                tokio::net::UnixListener::bind(a206_broker::socket_path(dir.path())).unwrap();
            let broker = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                for offered in (version..=a206_broker::BROKER_VERSION).rev() {
                    let frame = read_frame::<Frame<BrokerRequest>>(&mut socket)
                        .await
                        .unwrap()
                        .unwrap();
                    assert!(matches!(
                        frame.body,
                        BrokerRequest::Hello {
                            role: Role::User,
                            version: actual
                        } if actual == offered
                    ));
                    write_frame(
                        &mut socket,
                        &Response {
                            id: frame.id,
                            body: if offered == version {
                                BrokerReply::Ok
                            } else {
                                BrokerReply::Error(BrokerError::Invalid(
                                    InvalidRequest::UnsupportedVersion,
                                ))
                            },
                        },
                    )
                    .await
                    .unwrap();
                }
                // Refusal must happen before opening a Relay socket or sending setup opcodes.
                assert!(
                    read_frame::<Frame<BrokerRequest>>(&mut socket)
                        .await
                        .unwrap()
                        .is_none()
                );
            });
            assert!(matches!(
                connect(dir.path()).await,
                Err(crate::vault::VaultError::ChannelUnsupported)
            ));
            broker.await.unwrap();
        }
    }
}
