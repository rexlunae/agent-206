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
        let (sender, receiver) = mpsc::channel(32);
        std::thread::spawn(move || {
            let result = tokio::runtime::Runtime::new().and_then(|runtime| {
                runtime
                    .block_on(run(root, remote, receiver))
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
) -> Result<()> {
    while let Some(first) = receiver.recv().await {
        let child = crate::transport::gateway_command(&root, remote.as_deref())?.spawn();
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
            if crate::transport::write_frame(&mut stdin, &request)
                .await
                .is_err()
            {
                break;
            }
        }
        tokio::select! {
            _result = &mut reader => break,
            incoming = receiver.recv() => { next = incoming; if next.is_none() { break; } }
        }
    }
    reader.abort();
    for (_, sender) in pending.lock().await.drain() {
        let _delivery = sender.send(Err(anyhow::anyhow!("gateway connection was lost; the command may have completed; refresh state before retrying")));
    }
    let _kill_result = child.kill().await;
    Ok(stopped)
}
