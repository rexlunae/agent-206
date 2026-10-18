//! WebSocket sessions relayed for the gateway (`Request::SocketOpen`).
//!
//! The broker asks the service, with the credential, where to connect, and
//! connects there itself: the socket URL (which may carry a session ticket)
//! and any message carrying the credential stay in the broker. Upstream text
//! messages are relayed as numbered frames; the gateway's messages go back
//! upstream unchanged.
use super::{Inbox, Outbound, SocketOut, request, revoked, upstream, write};
use crate::proto::*;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::net::unix::OwnedWriteHalf;
use tokio_tungstenite::tungstenite::{self, Message, protocol::WebSocketConfig};
use zeroize::Zeroizing;

/// How long a connection and its handshake may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the broker pings the service.
const PING_EVERY: Duration = Duration::from_secs(30);
/// A session that hears nothing for this long (not even a pong) is dead.
const SILENCE_LIMIT: Duration = Duration::from_secs(90);
/// How long one upstream write may take.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest answer accepted from the service when asking where to connect.
const MAX_DISCOVERY_BODY: usize = 64 * 1024;
/// Discord gateway version and encoding, set on the URL Discord names.
const DISCORD_QUERY: [(&str, &str); 2] = [("v", "10"), ("encoding", "json")];

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

enum Opened {
    /// Connected, with the message to send after the service's first one
    /// (Discord's Identify), if the scheme has one.
    Socket(Box<Ws>, Option<Zeroizing<String>>),
    Refused {
        status: u16,
        code: Option<String>,
    },
}

/// Where the service says to connect. Discord answers with `url`; Slack with
/// `ok`, and `url` or an `error` code.
#[derive(Deserialize)]
struct Named {
    #[serde(default)]
    ok: Option<bool>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Serialize)]
struct Identify<'a> {
    op: u8,
    d: IdentifyData<'a>,
}
#[derive(Serialize)]
struct IdentifyData<'a> {
    token: &'a str,
    intents: u64,
    properties: Properties,
}
#[derive(Serialize)]
struct Properties {
    os: &'static str,
    browser: &'static str,
    device: &'static str,
}

/// A WebSocket scheme: how to ask where to connect, and what to say first.
#[derive(Clone, Copy)]
enum Web {
    Discord { intents: u64 },
    Slack,
}

/// The first message Discord sends: `{"op": 10, ...}` (Hello).
#[derive(Deserialize)]
struct Opcode {
    op: Option<u64>,
}

/// Serve one socket session as the rest of the connection, ending with
/// `SocketRefused`, `SocketClosed` or an `Error`. Returns early, without a
/// final frame, when the gateway went away or the broker is stopping.
pub(super) async fn tunnel(
    shared: &super::Shared,
    out: SocketOut,
    id: uuid::Uuid,
    writer: &mut OwnedWriteHalf,
    inbox: &mut Inbox,
) -> std::io::Result<()> {
    let web = match &out.scheme {
        SocketScheme::IrcSasl { login } => {
            let login = login.clone();
            return super::irc::tunnel(shared, out, login, id, writer, inbox).await;
        }
        SocketScheme::DiscordGateway { intents } => Web::Discord { intents: *intents },
        SocketScheme::SlackSocketMode => Web::Slack,
    };
    let cancel = out.cancel.clone();
    let opened = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(revoked(shared)),
        _ = shared.stop.cancelled() => return Ok(()),
        closed = inbox.closed() => return closed,
        opened = open(shared, &out, web) => opened,
    };
    drop(out);
    let (mut ws, mut identify) = match opened {
        Ok(Opened::Socket(ws, identify)) => (ws, identify),
        Ok(Opened::Refused { status, code }) => {
            return write(writer, id, Reply::SocketRefused { status, code }).await;
        }
        Err(error) => {
            tracing::info!(
                error = &error as &dyn std::error::Error,
                "vault broker could not open a socket"
            );
            return write(writer, id, Reply::Error(error)).await;
        }
    };
    write(writer, id, Reply::SocketOpened).await?;
    let mut seq = 0u64;
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + PING_EVERY, PING_EVERY);
    let mut silent_until = tokio::time::Instant::now() + SILENCE_LIMIT;
    let error = loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break revoked(shared),
            _ = shared.stop.cancelled() => return Ok(()),
            request = inbox.next() => {
                // The gateway closed the connection: dropping the socket closes it.
                let Some(request) = request? else { return Ok(()) };
                match request.body {
                    Request::SocketSend { text } if text.len() <= MAX_SOCKET_MESSAGE => {
                        if let Err(error) = send(&mut ws, Message::text(text)).await {
                            break error;
                        }
                    }
                    Request::SocketSend { .. } => {
                        break BrokerError::Invalid(InvalidRequest::SocketMessage);
                    }
                    _ => {
                        let busy = BrokerError::Invalid(InvalidRequest::SocketBusy);
                        write(writer, request.id, Reply::Error(busy)).await?;
                    }
                }
            }
            _ = ping.tick() => {
                if let Err(error) = send(&mut ws, Message::Ping(Default::default())).await {
                    break error;
                }
            }
            () = tokio::time::sleep_until(silent_until) => {
                break BrokerError::Upstream(UpstreamFailure::Timeout);
            }
            message = ws.next() => {
                silent_until = tokio::time::Instant::now() + SILENCE_LIMIT;
                let text = match message {
                    Some(Ok(Message::Text(text))) => text.as_str().to_owned(),
                    Some(Ok(Message::Close(frame))) => {
                        let code = frame.map(|frame| u16::from(frame.code));
                        return write(writer, id, Reply::SocketClosed { code }).await;
                    }
                    Some(Ok(Message::Binary(_))) => {
                        break BrokerError::Upstream(UpstreamFailure::SocketProtocol);
                    }
                    // Pings are answered by the WebSocket layer.
                    Some(Ok(_)) => continue,
                    Some(Err(error)) => break socket_error(error),
                    None => break BrokerError::Upstream(UpstreamFailure::Transfer),
                };
                let first = identify.take();
                if first.is_some() && !is_hello(&text) {
                    break BrokerError::Upstream(UpstreamFailure::SocketProtocol);
                }
                let frame = Reply::SocketFrame { seq, text };
                // A gateway that stops reading must not keep a revoked session alive.
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Ok(()),
                    written = write(writer, id, frame) => written?,
                }
                seq += 1;
                if let Some(first) = first {
                    // The frame's buffer is not zeroized; it lives only until
                    // it is written to the TLS stream.
                    if let Err(error) = send(&mut ws, Message::text(first.as_str())).await {
                        break error;
                    }
                }
            }
        }
    };
    tracing::info!(
        error = &error as &dyn std::error::Error,
        "vault broker ended a socket session"
    );
    write(writer, id, Reply::Error(error)).await
}

fn is_hello(text: &str) -> bool {
    serde_json::from_str::<Opcode>(text).is_ok_and(|opcode| opcode.op == Some(10))
}

async fn send(ws: &mut Ws, message: Message) -> Result<(), BrokerError> {
    match tokio::time::timeout(SEND_TIMEOUT, ws.send(message)).await {
        Ok(sent) => sent.map_err(socket_error),
        Err(_) => Err(BrokerError::Upstream(UpstreamFailure::Timeout)),
    }
}

fn socket_error(error: tungstenite::Error) -> BrokerError {
    tracing::warn!(
        error = &error as &dyn std::error::Error,
        "socket transfer failed"
    );
    BrokerError::Upstream(match error {
        tungstenite::Error::Capacity(_) => UpstreamFailure::TooLarge,
        tungstenite::Error::Protocol(_) | tungstenite::Error::Utf8(_) => {
            UpstreamFailure::SocketProtocol
        }
        _ => UpstreamFailure::Transfer,
    })
}

/// Ask the service where to connect, then connect there.
async fn open(shared: &super::Shared, out: &SocketOut, web: Web) -> Result<Opened, BrokerError> {
    let (method, header) = match web {
        Web::Discord { .. } => (HttpMethod::Get, format!("Bot {}", out.token.as_str())),
        Web::Slack => (HttpMethod::Post, format!("Bearer {}", out.token.as_str())),
    };
    let response = request(
        shared.client(&out.url),
        Outbound {
            method,
            url: out.url.clone(),
            header: Some(("authorization", Zeroizing::new(header))),
            anthropic_version: None,
            accept_json: false,
            body: None,
            cancel: out.cancel.clone(),
            stream: false,
        },
    )
    .await?;
    let status = response.status();
    // Never relay upstream error bodies: they can echo credentials.
    if !status.is_success() {
        return Ok(Opened::Refused {
            status: status.as_u16(),
            code: None,
        });
    }
    let mut body = response.bytes_stream();
    let mut named = vec![];
    while let Some(part) = body.next().await {
        let part = part.map_err(upstream)?;
        if named.len() + part.len() > MAX_DISCOVERY_BODY {
            return Err(BrokerError::Upstream(UpstreamFailure::TooLarge));
        }
        named.extend_from_slice(&part);
    }
    let named: Named = serde_json::from_slice(&named)
        .map_err(|_| BrokerError::Upstream(UpstreamFailure::SocketProtocol))?;
    if named.ok == Some(false) {
        return Ok(Opened::Refused {
            status: status.as_u16(),
            code: named.error.filter(|code| valid_code(code)),
        });
    }
    let url = named
        .url
        .ok_or(BrokerError::Upstream(UpstreamFailure::SocketProtocol))?;
    let mut url = socket_url(&out.url, &url)?;
    let identify = match web {
        Web::Discord { intents } => {
            url.query_pairs_mut().clear().extend_pairs(DISCORD_QUERY);
            let identify = Identify {
                op: 2,
                d: IdentifyData {
                    token: &out.token,
                    intents,
                    properties: Properties {
                        os: std::env::consts::OS,
                        browser: "agent206",
                        device: "agent206",
                    },
                },
            };
            let identify = serde_json::to_string(&identify)
                .map_err(|_| BrokerError::Upstream(UpstreamFailure::InvalidHeader))?;
            Some(Zeroizing::new(identify))
        }
        Web::Slack => None,
    };
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_SOCKET_MESSAGE))
        .max_frame_size(Some(MAX_SOCKET_MESSAGE));
    // A system proxy is not used: the socket connects directly, over TLS
    // unless the service is on loopback.
    let connected = tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio_tungstenite::connect_async_with_config(url.as_str(), Some(config), true),
    )
    .await
    .map_err(|_| BrokerError::Upstream(UpstreamFailure::Timeout))?;
    let (ws, _) = connected.map_err(|error| match error {
        tungstenite::Error::Io(_) | tungstenite::Error::Tls(_) => {
            tracing::warn!(
                error = &error as &dyn std::error::Error,
                "socket connection failed"
            );
            BrokerError::Upstream(UpstreamFailure::Connect)
        }
        error => socket_error(error),
    })?;
    Ok(Opened::Socket(Box::new(ws), identify))
}

/// The socket URL a service named. It must be `wss`, or `ws` on loopback
/// when the service itself was reached over loopback HTTP, so the session is
/// never less protected than the request that named it.
fn socket_url(origin: &url::Url, named: &str) -> Result<url::Url, BrokerError> {
    let refused = BrokerError::Upstream(UpstreamFailure::SocketUrl);
    let url = url::Url::parse(named).map_err(|_| refused.clone())?;
    let loopback = |url: &url::Url| {
        url.host_str()
            .is_some_and(|h| matches!(h, "localhost" | "127.0.0.1" | "[::1]" | "::1"))
    };
    let allowed = match url.scheme() {
        "wss" => true,
        "ws" => loopback(&url) && origin.scheme() == "http" && loopback(origin),
        _ => false,
    };
    if !allowed
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(refused);
    }
    Ok(url)
}

/// A service error code safe to relay: short, lowercase ASCII, digits and
/// underscores, so it cannot carry anything the service echoed.
fn valid_code(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= 64
        && code
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_urls_are_never_weaker_than_the_request_that_named_them() {
        let host = format!("{}.test", uuid::Uuid::new_v4().simple());
        let https = url::Url::parse(&format!("https://{host}/")).unwrap();
        let loopback = url::Url::parse("http://127.0.0.1:9/").unwrap();
        assert!(socket_url(&https, &format!("wss://{host}/")).is_ok());
        for named in [
            format!("ws://{host}/"),
            "ws://127.0.0.1:9/".to_owned(),
            format!("https://{host}/"),
            format!("wss://user:pass@{host}/"),
            format!("wss://{host}/#fragment"),
        ] {
            assert!(socket_url(&https, &named).is_err(), "{named}");
        }
        assert!(socket_url(&loopback, "ws://127.0.0.1:9/").is_ok());
        assert!(socket_url(&loopback, &format!("ws://{host}/")).is_err());
    }

    #[test]
    fn only_plain_error_codes_are_relayed() {
        assert!(valid_code("invalid_auth"));
        for code in ["", "Invalid", "has space", "xapp-1-", &"a".repeat(65)] {
            assert!(!valid_code(code), "{code}");
        }
    }
}
