//! Dedicated gateway I/O worker. Reconnect unsent work; never repeat an accepted mutation.
use a206_core::{Command, Reply};
use a206_protocol::{Request, Response};
use anyhow::{Context, Result};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::sync::{Mutex, mpsc, oneshot};
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
            let result = tokio::runtime::Runtime::new().and_then(|runtime| {
                runtime
                    .block_on(run(root, remote, receiver, start, explicit))
                    .map_err(std::io::Error::other)
            });
            if let Err(error) = result {
                eprintln!("gateway connection ended: {error}");
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
            .context("gateway disconnected")?;
        receiver
            .await
            .context("gateway reply lost; refresh before repeating an action")?
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
        let child =
            crate::transport::gateway_command(&root, remote.as_deref(), start, explicit)?.spawn();
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
                let _delivery = first.1.send(Err(error.into()));
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
    let mut stdin = child.stdin.take().context("gateway stdin missing")?;
    let mut stdout = child.stdout.take().context("gateway stdout missing")?;
    let mut stderr = child.stderr.take().context("gateway stderr missing")?;
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
                let _delivery = sender.send(Ok(response.body));
            }
        }
        Ok::<(), anyhow::Error>(())
    });
    let mut next = Some(first);
    let mut stopped = false;
    loop {
        if let Some((request, sender)) = next.take() {
            let mut outstanding = pending.lock().await;
            outstanding.retain(|_, sender| !sender.is_closed());
            if outstanding.len() >= 32 {
                let _delivery = sender.send(Err(anyhow::anyhow!(
                    "gateway has too many outstanding requests"
                )));
                continue;
            }
            if let std::collections::hash_map::Entry::Vacant(entry) = outstanding.entry(request.id)
            {
                entry.insert(sender);
            } else {
                let _delivery =
                    sender.send(Err(anyhow::anyhow!("request id is already in flight")));
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
    let _kill_result = child.kill().await;
    let detail = tokio::time::timeout(std::time::Duration::from_secs(1), &mut diagnostics)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    diagnostics.abort();
    for (_, sender) in pending.lock().await.drain() {
        let _delivery = sender.send(Err(anyhow::anyhow!("gateway connection was lost; the command may have completed; refresh state before retrying{}",
            if detail.is_empty() { String::new() } else { format!(": {detail}") })));
    }
    Ok(stopped)
}
