//! A one-use SSH agent for an OpenSSH client the gateway runs: it answers
//! only that client process, offers the one key the call uses, and passes
//! each request to sign to a [`Signer`] (the vault broker, which holds the
//! private key and signs only authentication requests for it). The
//! protocol is draft-ietf-sshm-ssh-agent; everything but listing keys and
//! signing is refused.
use futures_util::future::BoxFuture;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::{mpsc, watch},
};

/// Largest agent message accepted (the protocol's own limit).
pub const MAX_MESSAGE: usize = 256 * 1024;
/// Most connections one agent accepts: an SSH client opens one.
const MAX_CONNECTIONS: usize = 4;
/// Most signatures one agent makes: a client signs once per key it tries.
const MAX_SIGNATURES: usize = 3;

const FAILURE: u8 = 5;
const REQUEST_IDENTITIES: u8 = 11;
const IDENTITIES_ANSWER: u8 = 12;
const SIGN_REQUEST: u8 = 13;
const SIGN_RESPONSE: u8 = 14;

/// Signs data for the agent's key.
pub trait Signer: Send + 'static {
    type Error: Send + 'static;
    fn sign(&mut self, data: Vec<u8>) -> BoxFuture<'_, Result<Vec<u8>, Self::Error>>;
}

/// What the agent offers and answers.
pub struct Agent<S: Signer> {
    /// The key in SSH wire format.
    pub key: Vec<u8>,
    pub comment: String,
    pub signer: S,
    /// The client's process id once it has started; until then, and for
    /// any other process, connections are refused.
    pub client: watch::Receiver<Option<u32>>,
    /// Where signing failures go, so the call reports the cause instead of
    /// the client's "permission denied".
    pub failures: mpsc::Sender<S::Error>,
}

/// Serve `listener` until it is dropped or the task is aborted.
pub async fn serve<S: Signer>(listener: UnixListener, mut agent: Agent<S>) {
    let mut signatures = 0;
    for _ in 0..MAX_CONNECTIONS {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let expected = *agent.client.borrow();
        let peer = stream.peer_cred().ok().and_then(|c| c.pid());
        if expected.is_none() || peer.and_then(|p| u32::try_from(p).ok()) != expected {
            tracing::warn!(
                ?peer,
                "refused an SSH agent connection from another process"
            );
            continue;
        }
        if let Err(error) = answer(stream, &mut agent, &mut signatures).await {
            // The client hung up or broke the protocol; it reports its own
            // failure, and the call reports the client's.
            tracing::debug!(
                error = &error as &dyn std::error::Error,
                "SSH agent connection ended"
            );
        }
    }
}

async fn answer<S: Signer>(
    mut stream: UnixStream,
    agent: &mut Agent<S>,
    signatures: &mut usize,
) -> std::io::Result<()> {
    loop {
        let mut length = [0; 4];
        match stream.read_exact(&mut length).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        }
        let length = u32::from_be_bytes(length) as usize;
        if length == 0 || length > MAX_MESSAGE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "agent message length out of range",
            ));
        }
        let mut message = vec![0; length];
        stream.read_exact(&mut message).await?;
        let reply = match message[0] {
            REQUEST_IDENTITIES => {
                let mut reply = vec![IDENTITIES_ANSWER];
                reply.extend(1u32.to_be_bytes());
                put(&mut reply, &agent.key);
                put(&mut reply, agent.comment.as_bytes());
                reply
            }
            SIGN_REQUEST => sign(&message[1..], agent, signatures).await,
            _ => vec![FAILURE],
        };
        let mut framed = Vec::with_capacity(reply.len() + 4);
        put(&mut framed, &reply);
        stream.write_all(&framed).await?;
    }
}

async fn sign<S: Signer>(body: &[u8], agent: &mut Agent<S>, signatures: &mut usize) -> Vec<u8> {
    let mut rest = body;
    let (Some(key), Some(data), Some(_flags)) = (
        string(&mut rest),
        string(&mut rest),
        rest.get(..4).filter(|_| rest.len() == 4),
    ) else {
        return vec![FAILURE];
    };
    if key != agent.key.as_slice() || *signatures >= MAX_SIGNATURES {
        return vec![FAILURE];
    }
    *signatures += 1;
    match agent.signer.sign(data.to_vec()).await {
        Ok(signature) => {
            let mut reply = vec![SIGN_RESPONSE];
            put(&mut reply, &signature);
            reply
        }
        Err(error) => {
            // The receiver is gone only once the call has ended.
            if agent.failures.try_send(error).is_err() {
                tracing::debug!("an SSH signing failure arrived after its call ended");
            }
            vec![FAILURE]
        }
    }
}

/// Append an SSH `string`: a big-endian `uint32` length and the bytes.
pub fn put(out: &mut Vec<u8>, bytes: &[u8]) {
    // Every caller's bytes are bounded far below 4 GiB.
    out.extend((bytes.len() as u32).to_be_bytes());
    out.extend(bytes);
}

/// Take an SSH `string` from the front of `rest`.
pub fn string<'a>(rest: &mut &'a [u8]) -> Option<&'a [u8]> {
    let length = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
    let bytes = rest.get(4..4 + length)?;
    *rest = &rest[4 + length..];
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;
    impl Signer for Echo {
        type Error = String;
        fn sign(&mut self, data: Vec<u8>) -> BoxFuture<'_, Result<Vec<u8>, String>> {
            Box::pin(async move {
                if data.starts_with(b"bad") {
                    Err(String::from_utf8(data).unwrap())
                } else {
                    Ok([b"signed:".as_slice(), &data].concat())
                }
            })
        }
    }

    async fn exchange(stream: &mut UnixStream, message: &[u8]) -> Vec<u8> {
        let mut framed = vec![];
        put(&mut framed, message);
        stream.write_all(&framed).await.unwrap();
        let mut length = [0; 4];
        stream.read_exact(&mut length).await.unwrap();
        let mut reply = vec![0; u32::from_be_bytes(length) as usize];
        stream.read_exact(&mut reply).await.unwrap();
        reply
    }

    fn sign_request(key: &[u8], data: &[u8]) -> Vec<u8> {
        let mut message = vec![SIGN_REQUEST];
        put(&mut message, key);
        put(&mut message, data);
        message.extend(0u32.to_be_bytes());
        message
    }

    #[tokio::test]
    async fn the_agent_offers_its_key_and_signs_only_for_its_client() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent");
        let listener = UnixListener::bind(&path).unwrap();
        let key = uuid::Uuid::new_v4().as_bytes().to_vec();
        let comment = uuid::Uuid::new_v4().to_string();
        let (client, waiting) = watch::channel(None);
        let (failures, mut failed) = mpsc::channel(4);
        let task = tokio::spawn(serve(
            listener,
            Agent {
                key: key.clone(),
                comment: comment.clone(),
                signer: Echo,
                client: waiting,
                failures,
            },
        ));
        // Before the client is known, and for any other process, nothing
        // is answered.
        let mut early = UnixStream::connect(&path).await.unwrap();
        let mut byte = [0; 1];
        assert_eq!(early.read(&mut byte).await.unwrap(), 0);
        client.send(Some(std::process::id() + 1)).unwrap();
        let mut stranger = UnixStream::connect(&path).await.unwrap();
        assert_eq!(stranger.read(&mut byte).await.unwrap(), 0);

        client.send(Some(std::process::id())).unwrap();
        let mut stream = UnixStream::connect(&path).await.unwrap();
        let mut listed = vec![IDENTITIES_ANSWER];
        listed.extend(1u32.to_be_bytes());
        put(&mut listed, &key);
        put(&mut listed, comment.as_bytes());
        assert_eq!(exchange(&mut stream, &[REQUEST_IDENTITIES]).await, listed);
        let data = uuid::Uuid::new_v4().to_string();
        let mut signed = vec![SIGN_RESPONSE];
        put(&mut signed, format!("signed:{data}").as_bytes());
        assert_eq!(
            exchange(&mut stream, &sign_request(&key, data.as_bytes())).await,
            signed
        );
        // Another key, a malformed request or another message is refused.
        assert_eq!(
            exchange(&mut stream, &sign_request(b"other", data.as_bytes())).await,
            [FAILURE]
        );
        let mut truncated = sign_request(&key, data.as_bytes());
        truncated.pop();
        assert_eq!(exchange(&mut stream, &truncated).await, [FAILURE]);
        assert_eq!(exchange(&mut stream, &[27]).await, [FAILURE]);
        // A signing failure is refused to the client and reported.
        let bad = format!("bad{data}");
        assert_eq!(
            exchange(&mut stream, &sign_request(&key, bad.as_bytes())).await,
            [FAILURE]
        );
        assert_eq!(failed.recv().await.unwrap(), bad);
        // Signatures are bounded.
        assert_eq!(
            exchange(&mut stream, &sign_request(&key, data.as_bytes())).await,
            signed
        );
        assert_eq!(
            exchange(&mut stream, &sign_request(&key, data.as_bytes())).await,
            [FAILURE]
        );
        // An oversized message ends the connection.
        stream
            .write_all(&(MAX_MESSAGE as u32 + 1).to_be_bytes())
            .await
            .unwrap();
        assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
        task.abort();
    }
}
