//! A sequential broker connection. Each request waits for its own reply.
use crate::proto::*;
use a206_protocol::{Decoder, Request as Envelope, Response, encode};
use std::{collections::VecDeque, path::Path};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(
        "the running vault broker is too old for credential setup; run `agent206 vault stop`, then retry"
    )]
    CredentialsUnsupported,
    #[error(
        "the running vault broker is too old for reviewed import; run `agent206 vault stop`, then retry"
    )]
    ReviewedImportUnsupported,
    #[error("vault broker is not running or unreachable")]
    Io(#[from] std::io::Error),
    #[error("vault broker protocol error")]
    Protocol(#[from] a206_protocol::Error),
    #[error("vault broker closed the connection")]
    Closed,
    #[error("the running vault broker is too old to stream provider responses")]
    StreamUnsupported,
    #[error("the running vault broker is too old to open sockets")]
    SocketUnsupported,
    #[error("the vault broker sent an invalid provider stream")]
    Stream(#[from] StreamViolation),
    #[error("a socket message is larger than the vault broker relays")]
    SocketMessage,
    #[error(transparent)]
    Broker(#[from] BrokerError),
}

/// A streamed provider reply that breaks the framing contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StreamViolation {
    #[error("a stream chunk exceeds {MAX_STREAM_CHUNK} bytes")]
    ChunkTooLarge,
    #[error("stream chunks arrived out of order")]
    OutOfOrder,
    #[error("the stream body exceeds {MAX_PROVIDER_BODY} bytes")]
    BodyTooLarge,
    #[error("the stream ended with a chunk count or length that does not match")]
    Length,
    #[error("an unexpected reply arrived in the stream")]
    Unexpected,
}

pub struct Client {
    stream: tokio::net::UnixStream,
    decoder: Decoder,
    /// Replies decoded together with an earlier one, in arrival order.
    pending: VecDeque<Response<Reply>>,
    version: u16,
}

impl Client {
    /// Connect and greet with the newest protocol version the broker accepts,
    /// down to [`MIN_BROKER_VERSION`].
    pub async fn connect(root: &Path, role: Role) -> Result<Self, ClientError> {
        let mut client = Self::raw(root).await?;
        let hello = Request::Hello {
            role,
            version: BROKER_VERSION,
        };
        match client.call(hello).await {
            Ok(_) => {
                client.version = BROKER_VERSION;
                return Ok(client);
            }
            Err(ClientError::Broker(BrokerError::Invalid(InvalidRequest::UnsupportedVersion))) => {}
            Err(error) => return Err(error),
        }
        // A refused greeting leaves the connection ungreeted, so an older
        // version may be offered on it. A broker that refuses or drops every
        // offer is reported as incompatible, which names the recovery.
        for version in (MIN_BROKER_VERSION..BROKER_VERSION).rev() {
            if client.call(Request::Hello { role, version }).await.is_ok() {
                client.version = version;
                return Ok(client);
            }
        }
        Err(BrokerError::Invalid(InvalidRequest::UnsupportedVersion).into())
    }

    /// The protocol version this connection negotiated.
    pub fn version(&self) -> u16 {
        self.version
    }

    /// Stop whichever broker serves `root`, even one from another protocol
    /// version: current brokers accept `Shutdown` before `Hello`, and older
    /// ones (which refuse it as `Forbidden`) are greeted with their version.
    pub async fn shutdown(root: &Path) -> Result<(), ClientError> {
        match Self::raw(root).await?.call(Request::Shutdown).await {
            Err(ClientError::Broker(BrokerError::Forbidden)) => {}
            result => return result.map(drop),
        }
        for version in (1..BROKER_VERSION).rev() {
            let mut client = Self::raw(root).await?;
            let hello = Request::Hello {
                role: Role::User,
                version,
            };
            if client.call(hello).await.is_ok() {
                return client.call(Request::Shutdown).await.map(drop);
            }
        }
        Err(BrokerError::Invalid(InvalidRequest::UnsupportedVersion).into())
    }

    async fn raw(root: &Path) -> Result<Self, ClientError> {
        Ok(Self {
            stream: tokio::net::UnixStream::connect(crate::socket_path(root)).await?,
            decoder: Decoder::default(),
            pending: VecDeque::new(),
            version: 0,
        })
    }

    async fn send(&mut self, body: Request) -> Result<uuid::Uuid, ClientError> {
        let id = uuid::Uuid::new_v4();
        self.stream
            .write_all(&encode(&Envelope { id, body })?)
            .await?;
        Ok(id)
    }

    /// The next reply to request `id`. Replies to other (abandoned) requests
    /// are discarded. A broker error reply becomes `ClientError::Broker`.
    async fn recv(&mut self, id: uuid::Uuid) -> Result<Reply, ClientError> {
        let mut bytes = vec![0; 8192];
        loop {
            while let Some(reply) = self.pending.pop_front() {
                if reply.id == id {
                    return match reply.body {
                        Reply::Error(error) => Err(error.into()),
                        reply => Ok(reply),
                    };
                }
            }
            let count = self.stream.read(&mut bytes).await?;
            if count == 0 {
                return Err(ClientError::Closed);
            }
            let replies: Vec<Response<Reply>> = self.decoder.feed(&bytes[..count])?;
            self.pending.extend(replies);
        }
    }

    /// Send one request. A broker error reply becomes `ClientError::Broker`.
    pub async fn call(&mut self, body: Request) -> Result<Reply, ClientError> {
        if matches!(
            &body,
            Request::CredentialPage { .. } | Request::AddApiKey(_)
        ) && self.version < CREDENTIALS_VERSION
        {
            return Err(ClientError::CredentialsUnsupported);
        }
        if matches!(
            &body,
            Request::ReviewVaultImport { .. }
                | Request::ApplyVaultImport { .. }
                | Request::CancelVaultImport { .. }
        ) && self.version < REVIEWED_IMPORT_VERSION
        {
            return Err(ClientError::ReviewedImportUnsupported);
        }
        let id = self.send(body).await?;
        let mut reply = self.recv(id).await?;
        if self.version < SETUP_CHANNEL_VERSION
            && let Reply::Status(status) = &mut reply
        {
            // Older brokers counted minting a capability as paired, without
            // proof of receipt by a gateway. That fact is unknown to us.
            status.gateway_paired = None;
        }
        Ok(reply)
    }

    /// Start a streamed provider call and wait for the upstream status.
    /// Refused with [`ClientError::StreamUnsupported`] before anything is
    /// sent when the broker predates streaming; the caller may then use
    /// `Request::Provider`. Abandoning the stream early leaves its remaining
    /// frames on the connection, so drop the client with it.
    pub async fn provider_stream(
        &mut self,
        call: ProviderCall,
    ) -> Result<ProviderStream<'_>, ClientError> {
        if self.version < STREAM_VERSION {
            return Err(ClientError::StreamUnsupported);
        }
        let id = self.send(Request::ProviderStream(call)).await?;
        let Reply::ProviderHead { status } = self.recv(id).await? else {
            return Err(StreamViolation::Unexpected.into());
        };
        Ok(ProviderStream {
            client: self,
            id,
            status,
            chunks: 0,
            bytes: 0,
            ended: false,
        })
    }
}

/// The body of a streamed provider call, checked frame by frame.
pub struct ProviderStream<'a> {
    client: &'a mut Client,
    id: uuid::Uuid,
    status: u16,
    chunks: u64,
    bytes: u64,
    ended: bool,
}

impl ProviderStream<'_> {
    pub fn status(&self) -> u16 {
        self.status
    }

    /// The next body slice, or `None` once the broker confirmed the end.
    /// Cancel-safe only between calls: dropping a pending call may lose a
    /// frame, after which the stream must be abandoned.
    pub async fn next(&mut self) -> Result<Option<Vec<u8>>, ClientError> {
        if self.ended {
            return Ok(None);
        }
        match self.client.recv(self.id).await {
            Ok(Reply::ProviderChunk { seq, bytes }) => {
                if seq != self.chunks {
                    return Err(self.violation(StreamViolation::OutOfOrder));
                }
                if bytes.len() > MAX_STREAM_CHUNK {
                    return Err(self.violation(StreamViolation::ChunkTooLarge));
                }
                self.chunks += 1;
                self.bytes += bytes.len() as u64;
                if self.bytes > MAX_PROVIDER_BODY as u64 {
                    return Err(self.violation(StreamViolation::BodyTooLarge));
                }
                Ok(Some(bytes))
            }
            Ok(Reply::ProviderEnd { chunks, bytes }) => {
                self.ended = true;
                if (chunks, bytes) != (self.chunks, self.bytes) {
                    return Err(StreamViolation::Length.into());
                }
                Ok(None)
            }
            Ok(_) => Err(self.violation(StreamViolation::Unexpected)),
            Err(error) => {
                self.ended = true;
                Err(error)
            }
        }
    }

    fn violation(&mut self, violation: StreamViolation) -> ClientError {
        self.ended = true;
        violation.into()
    }
}

impl Client {
    /// Open a WebSocket or IRC session through the broker. The connection
    /// carries only this session from then on, so it is consumed. Refused
    /// with [`ClientError::SocketUnsupported`] before anything is sent when
    /// the broker predates sockets, or IRC for an IRC session.
    pub async fn socket(mut self, call: SocketCall) -> Result<SocketOpen, ClientError> {
        let needed = match call.scheme {
            SocketScheme::IrcSasl { .. } => IRC_VERSION,
            SocketScheme::DiscordGateway { .. } | SocketScheme::SlackSocketMode => SOCKET_VERSION,
        };
        if self.version < needed {
            return Err(ClientError::SocketUnsupported);
        }
        let id = self.send(Request::SocketOpen(call)).await?;
        match self.recv(id).await? {
            Reply::SocketOpened => Ok(SocketOpen::Opened(Socket {
                client: self,
                id,
                seq: 0,
                ended: false,
            })),
            Reply::SocketRefused { status, code } => Ok(SocketOpen::Refused { status, code }),
            _ => Err(StreamViolation::Unexpected.into()),
        }
    }
}

/// The outcome of [`Client::socket`].
pub enum SocketOpen {
    Opened(Socket),
    /// The service refused to name a socket (see `Reply::SocketRefused`).
    Refused {
        status: u16,
        code: Option<String>,
    },
}

/// Something an open socket delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketEvent {
    Text(String),
    /// The service closed the socket, with its close code if it sent one.
    Closed(Option<u16>),
}

/// A WebSocket session relayed by the broker.
pub struct Socket {
    client: Client,
    id: uuid::Uuid,
    seq: u64,
    ended: bool,
}

impl Socket {
    /// The next upstream message, or `Closed` once. After `Closed` or an
    /// error the session is over, and later calls report
    /// [`ClientError::Closed`]. Cancel-safe: a dropped call loses nothing.
    pub async fn next(&mut self) -> Result<SocketEvent, ClientError> {
        if self.ended {
            return Err(ClientError::Closed);
        }
        let reply = self.client.recv(self.id).await;
        let event = match reply {
            Ok(Reply::SocketFrame { seq, text }) if seq == self.seq => {
                if text.len() > MAX_SOCKET_MESSAGE {
                    Err(StreamViolation::BodyTooLarge.into())
                } else {
                    self.seq += 1;
                    return Ok(SocketEvent::Text(text));
                }
            }
            Ok(Reply::SocketFrame { .. }) => Err(StreamViolation::OutOfOrder.into()),
            Ok(Reply::SocketClosed { code }) => Ok(SocketEvent::Closed(code)),
            Ok(_) => Err(StreamViolation::Unexpected.into()),
            Err(error) => Err(error),
        };
        self.ended = true;
        event
    }

    /// Send one text message upstream. Not cancel-safe: a dropped call may
    /// leave part of a frame written, after which the socket must be dropped.
    pub async fn send(&mut self, text: String) -> Result<(), ClientError> {
        if text.len() > MAX_SOCKET_MESSAGE {
            return Err(ClientError::SocketMessage);
        }
        self.client
            .send(Request::SocketSend { text })
            .await
            .map(drop)
    }
}
