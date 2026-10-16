//! Dedicated gateway I/O worker. Reconnect unsent work; never repeat an accepted mutation.
use a206_core::{Command, ErrorKind, Ignore, Reply};
use a206_protocol::{Request, Response};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::sync::{Mutex, mpsc, oneshot};
/// Why a gateway call did not produce a reply. A command the gateway ran and
/// rejected is `Ok(Reply::Error(..))`, not one of these.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("gateway disconnected")]
    Disconnected,
    #[error("gateway reply lost; refresh before repeating an action")]
    ReplyLost,
    #[error("could not start the gateway connection runtime")]
    Runtime(#[source] std::io::Error),
    #[error(transparent)]
    Launch(#[from] crate::transport::LaunchError),
    #[error("could not launch the gateway connection")]
    Spawn(#[source] std::io::Error),
    #[error("gateway {0} pipe missing")]
    Pipe(&'static str),
    #[error("gateway has too many outstanding requests")]
    Busy,
    #[error("request id is already in flight")]
    InFlight,
    #[error(
        "gateway connection was lost; the command may have completed; refresh state before retrying"
    )]
    Lost {
        /// The gateway's last diagnostics, kept as a cause rather than message text.
        #[source]
        diagnostics: Option<Diagnostics>,
    },
}
impl crate::error::Classified for ClientError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::Disconnected
            | Self::ReplyLost
            | Self::Spawn(_)
            | Self::Busy
            | Self::Lost { .. } => ErrorKind::Unavailable,
            Self::InFlight => ErrorKind::Conflict,
            Self::Runtime(_) | Self::Pipe(_) => ErrorKind::Internal,
            // `transparent` hides the launch error from the chain.
            Self::Launch(error) => return crate::error::classify(error),
        })
    }
}
/// The tail of the gateway's standard error, shown as received.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Diagnostics(pub String);
type Result<T, E = ClientError> = std::result::Result<T, E>;
type Call = (Request<Command>, oneshot::Sender<Result<Reply>>);
type Pending = Arc<Mutex<HashMap<uuid::Uuid, oneshot::Sender<Result<Reply>>>>>;
#[derive(Clone)]
pub struct Client {
    sender: mpsc::Sender<Call>,
}
impl Client {
    pub fn start(root: PathBuf, remote: Option<String>) -> Self {
        Self::connect(root, remote, true, false)
    }
    pub fn attach(root: PathBuf, remote: Option<String>) -> Self {
        Self::connect(root, remote, false, false)
    }
    pub fn launch(root: PathBuf, remote: Option<String>) -> Self {
        Self::connect(root, remote, true, true)
    }
    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }
    fn connect(root: PathBuf, remote: Option<String>, start: bool, explicit: bool) -> Self {
        let (sender, receiver) = mpsc::channel(32);
        std::thread::spawn(move || {
            let result = tokio::runtime::Runtime::new()
                .map_err(ClientError::Runtime)
                .and_then(|runtime| runtime.block_on(run(root, remote, receiver, start, explicit)));
            if let Err(error) = result {
                crate::error::handle("gateway_connection", &error);
            }
        });
        Self { sender }
    }
    pub async fn call(&self, body: Command) -> Result<Reply> {
        self.call_with_id(uuid::Uuid::new_v4(), body).await
    }
    pub async fn call_with_id(&self, id: uuid::Uuid, body: Command) -> Result<Reply> {
        let (sender, receiver) = oneshot::channel();
        self.sender
            .send((Request { id, body }, sender))
            .await
            .map_err(|_| ClientError::Disconnected)?;
        receiver.await.map_err(|_| ClientError::ReplyLost)?
    }
}
async fn run(
    root: PathBuf,
    remote: Option<String>,
    mut receiver: mpsc::Receiver<Call>,
    mut start: bool,
    mut explicit: bool,
) -> Result<()> {
    while let Some(first) = receiver.recv().await {
        let child = crate::transport::gateway_command(&root, remote.as_deref(), start, explicit)
            .map_err(ClientError::Launch)
            .and_then(|mut command| command.spawn().map_err(ClientError::Spawn));
        // Only initial connection or an explicit user Start may launch a service.
        start = false;
        explicit = false;
        match child {
            Ok(child) => {
                let stopped = connection(child, first, &mut receiver).await?;
                // An explicit stop remains stopped until the user reconnects the client.
                if stopped {
                    return Ok(());
                }
            }
            Err(error) => {
                // The caller stopped waiting; there is no one left to tell.
                first.1.send(Err(error)).ignore();
            }
        }
    }
    Ok(())
}
async fn connection(
    mut child: tokio::process::Child,
    first: Call,
    receiver: &mut mpsc::Receiver<Call>,
) -> Result<bool> {
    let mut stdin = child.stdin.take().ok_or(ClientError::Pipe("stdin"))?;
    let mut stdout = child.stdout.take().ok_or(ClientError::Pipe("stdout"))?;
    let mut stderr = child.stderr.take().ok_or(ClientError::Pipe("stderr"))?;
    let mut diagnostics = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut tail = std::collections::VecDeque::with_capacity(8192);
        let mut bytes = [0; 1024];
        while let Ok(count) = stderr.read(&mut bytes).await {
            if count == 0 {
                break;
            }
            tail.extend(&bytes[..count]);
            while tail.len() > 8192 {
                tail.pop_front();
            }
        }
        String::from_utf8_lossy(&tail.into_iter().collect::<Vec<_>>())
            .trim()
            .to_owned()
    });
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let reads = pending.clone();
    let mut reader = tokio::spawn(async move {
        while let Some(response) =
            crate::transport::read_frame::<Response<Reply>>(&mut stdout).await?
        {
            if let Some(sender) = reads.lock().await.remove(&response.id) {
                // The caller stopped waiting; there is no one left to tell.
                sender.send(Ok(response.body)).ignore();
            }
        }
        Ok::<(), crate::transport::FrameError>(())
    });
    let mut next = Some(first);
    let mut stopped = false;
    loop {
        if let Some((request, sender)) = next.take() {
            let mut outstanding = pending.lock().await;
            outstanding.retain(|_, sender| !sender.is_closed());
            if outstanding.len() >= 32 {
                // The caller stopped waiting; there is no one left to tell.
                sender.send(Err(ClientError::Busy)).ignore();
                continue;
            }
            if let std::collections::hash_map::Entry::Vacant(entry) = outstanding.entry(request.id)
            {
                entry.insert(sender);
            } else {
                // The caller stopped waiting; there is no one left to tell.
                sender.send(Err(ClientError::InFlight)).ignore();
                continue;
            }
            drop(outstanding);
            stopped |= matches!(request.body, Command::Shutdown);
            if !matches!(
                tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    crate::transport::write_frame(&mut stdin, &request)
                )
                .await,
                Ok(Ok(()))
            ) {
                break;
            }
        }
        tokio::select! {
            _result = &mut reader => break,
            incoming = receiver.recv() => { next = incoming; if next.is_none() { break; } }
        }
    }
    reader.abort();
    // The connection already failed; a child that already exited needs no kill.
    child.kill().await.ignore();
    let detail = tokio::time::timeout(std::time::Duration::from_secs(1), &mut diagnostics)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    diagnostics.abort();
    for (_, sender) in pending.lock().await.drain() {
        let lost = ClientError::Lost {
            diagnostics: (!detail.is_empty()).then(|| Diagnostics(detail.clone())),
        };
        // The caller stopped waiting; there is no one left to tell.
        sender.send(Err(lost)).ignore();
    }
    Ok(stopped)
}
