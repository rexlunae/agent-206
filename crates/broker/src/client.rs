//! A sequential broker connection. Each request waits for its own reply.
use crate::proto::*;
use a206_protocol::{Decoder, Request as Envelope, Response, encode};
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("vault broker is not running or unreachable: {0}")]
    Io(#[from] std::io::Error),
    #[error("vault broker protocol error: {0}")]
    Protocol(#[from] a206_protocol::Error),
    #[error("vault broker closed the connection")]
    Closed,
    #[error("{0}")]
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
