//! A sequential broker connection. Each request waits for its own reply.
use crate::proto::*;
use a206_protocol::{Decoder, Request as Envelope, Response, encode};
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("vault broker is not running or unreachable")]
    Io(#[from] std::io::Error),
    #[error("vault broker protocol error")]
    Protocol(#[from] a206_protocol::Error),
    #[error("vault broker closed the connection")]
    Closed,
    #[error(transparent)]
    Broker(#[from] BrokerError),
}

pub struct Client {
    stream: tokio::net::UnixStream,
    decoder: Decoder,
}

impl Client {
    pub async fn connect(root: &Path, role: Role) -> Result<Self, ClientError> {
        let stream = tokio::net::UnixStream::connect(crate::socket_path(root)).await?;
        let mut client = Self {
            stream,
            decoder: Decoder::default(),
        };
        client
            .call(Request::Hello {
                role,
                version: BROKER_VERSION,
            })
            .await?;
        Ok(client)
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
        })
    }

    /// Send one request. A broker error reply becomes `ClientError::Broker`.
    pub async fn call(&mut self, body: Request) -> Result<Reply, ClientError> {
        let id = uuid::Uuid::new_v4();
        self.stream
            .write_all(&encode(&Envelope { id, body })?)
            .await?;
        let mut bytes = vec![0; 8192];
        loop {
            let count = self.stream.read(&mut bytes).await?;
            if count == 0 {
                return Err(ClientError::Closed);
            }
            let replies: Vec<Response<Reply>> = self.decoder.feed(&bytes[..count])?;
            if let Some(reply) = replies.into_iter().find(|r| r.id == id) {
                return match reply.body {
                    Reply::Error(error) => Err(error.into()),
                    reply => Ok(reply),
                };
            }
        }
    }
}
