//! User-owned gateway lifecycle, shared by desktop, web bridge and CLI.
//! This actor runs outside the UI executor and never exposes its controls as agent tools.
use crate::client::Client;
use a206_core::{Command, GatewayAction, GatewayInfo, GatewayPhase, GatewayReport, Reply};
use anyhow::{Context, Result};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(25);
type ControlCall = (
    uuid::Uuid,
    GatewayAction,
    oneshot::Sender<Result<GatewayReport>>,
);

#[derive(Clone)]
pub struct GatewayController {
    client: Arc<Mutex<Client>>,
    controls: mpsc::Sender<ControlCall>,
}
impl GatewayController {
    pub fn new(root: PathBuf, remote: Option<String>) -> Self {
        Self::create(root, remote, true)
    }
    pub fn attach(root: PathBuf, remote: Option<String>) -> Self {
        Self::create(root, remote, false)
    }
    fn create(root: PathBuf, remote: Option<String>, auto_start: bool) -> Self {
        let client = Arc::new(Mutex::new(if auto_start {
            Client::start(root.clone(), remote.clone())
        } else {
            Client::attach(root.clone(), remote.clone())
        }));
        let (controls, mut receiver) = mpsc::channel::<ControlCall>(32);
        let shared = client.clone();
        std::thread::spawn(move || {
            let runtime = match tokio::runtime::Runtime::new() {
                Ok(runtime) => runtime,
                Err(_) => return,
            };
            runtime.block_on(async move {
                let mut stopped = remote.is_none() && crate::service::is_stopped(&root);
                let mut receipts = std::collections::BTreeMap::<uuid::Uuid, (GatewayAction, GatewayReport)>::new();
                while let Some((id, action, response)) = receiver.recv().await {
                    let mutation = matches!(action, GatewayAction::Start | GatewayAction::Stop | GatewayAction::Restart);
                    if mutation {
                        if let Some((previous, report)) = receipts.get(&id) {
                            let _delivery = response.send(if *previous == action { Ok(report.clone()) } else { Err(anyhow::anyhow!("control request ID reused with a different operation")) });
                            continue;
                        }
                        if receipts.len() >= 4096 {
                            let _delivery = response.send(Err(anyhow::anyhow!("control receipt limit reached; reopen the controller before submitting more lifecycle changes")));
                            continue;
                        }
                    }
                    let outcome = tokio::time::timeout(
                        CONTROL_TIMEOUT,
                        manage(&shared, &root, &remote, action, &mut stopped),
                    )
                    .await;
                    let result = outcome.unwrap_or_else(|_| {
                        Err(anyhow::anyhow!(
                            "gateway operation timed out; check status before retrying"
                        ))
                    });
                    let report = match result {
                        Ok(info) => GatewayReport {
                            target: target(&root, &remote),
                            phase: if info.is_some() {
                                GatewayPhase::Running
                            } else {
                                GatewayPhase::Stopped
                            },
                            info,
                            error: None,
                        },
                        Err(error) => {
                            // A persisted local stop is authoritative even for another client's stop.
                            let known_stopped =
                                stopped || (remote.is_none() && crate::service::is_stopped(&root));
                            GatewayReport {
                                target: target(&root, &remote),
                                phase: if known_stopped {
                                    GatewayPhase::Stopped
                                } else {
                                    GatewayPhase::Offline
                                },
                                info: None,
                                error: if action == GatewayAction::Status && known_stopped {
                                    None
                                } else {
                                    Some(format!("{error:#}"))
                                },
                            }
                        }
                    };
                    if mutation { receipts.insert(id, (action, report.clone())); }
                    let _delivery = response.send(Ok(report));
                }
            });
        });
        Self { client, controls }
    }
    pub async fn control(&self, action: GatewayAction) -> Result<GatewayReport> {
        self.control_with_id(uuid::Uuid::new_v4(), action).await
    }
    pub async fn control_with_id(
        &self,
        id: uuid::Uuid,
        action: GatewayAction,
    ) -> Result<GatewayReport> {
        let (sender, receiver) = oneshot::channel();
        self.controls
            .send((id, action, sender))
            .await
            .context("gateway controller unavailable")?;
        receiver.await.context("gateway controller disconnected")?
    }
    pub async fn call(&self, command: Command) -> Result<Reply> {
        current(&self.client)?.call(command).await
    }
    pub async fn call_with_id(&self, id: uuid::Uuid, command: Command) -> Result<Reply> {
        current(&self.client)?.call_with_id(id, command).await
    }
}
fn target(root: &std::path::Path, remote: &Option<String>) -> String {
    remote
        .clone()
        .unwrap_or_else(|| format!("Local · {}", root.display()))
}
fn current(shared: &Mutex<Client>) -> Result<Client> {
    Ok(shared
        .lock()
        .map_err(|_| anyhow::anyhow!("gateway connection lock poisoned"))?
        .clone())
}
fn replace(shared: &Mutex<Client>, client: Client) -> Result<()> {
    *shared
        .lock()
        .map_err(|_| anyhow::anyhow!("gateway connection lock poisoned"))? = client;
    Ok(())
}
async fn probe(client: Client) -> Result<GatewayInfo> {
    match client.call(Command::GatewayStatus).await? {
        Reply::Gateway(info) => Ok(info),
        Reply::Error { message } => anyhow::bail!("{message}"),
        _ => anyhow::bail!("gateway did not return status; update the gateway executable"),
    }
}
async fn stop(
    shared: &Mutex<Client>,
    root: &std::path::Path,
    remote: &Option<String>,
) -> Result<()> {
    if remote.is_none() {
        loop {
            if crate::service::stop_if_offline(root)? {
                replace(shared, Client::attach(root.into(), None))?;
                return Ok(());
            }
            // A starting daemon may own the profile lock before binding its
            // socket. Wait for it rather than mistaking that gap for an offline
            // profile, or starting another daemon to deliver the stop request.
            if current(shared)?.call(Command::GatewayStatus).await.is_ok() {
                break;
            }
            replace(shared, Client::attach(root.into(), None))?;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    let shutdown = match current(shared)?.call(Command::Shutdown).await {
        Ok(Reply::Ok) => Ok(()),
        Ok(Reply::Error { message }) => Err(anyhow::anyhow!(message)),
        Ok(_) => Err(anyhow::anyhow!("gateway did not acknowledge shutdown")),
        Err(error) => Err(error),
    };
    if let Err(error) = shutdown {
        // Another local controller can finish stop between our status probe and
        // shutdown write. Verify both its durable intent and released ownership;
        // a lost connection alone is never evidence of a successful stop.
        if remote.is_some() || !crate::service::is_stopped(root) {
            return Err(error);
        }
        loop {
            if crate::service::stop_if_offline(root)? {
                replace(shared, Client::attach(root.into(), None))?;
                return Ok(());
            }
            if !crate::service::is_stopped(root) {
                return Err(error);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    // Do not race restart against the previous process's socket and store lock.
    loop {
        let client = Client::attach(root.into(), remote.clone());
        if probe(client).await.is_err() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if remote.is_none() {
        loop {
            let lock = crate::store::profile_lock(root)?;
            if fs2::FileExt::try_lock_exclusive(&lock).is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    replace(shared, Client::attach(root.into(), remote.clone()))?;
    Ok(())
}
async fn manage(
    shared: &Mutex<Client>,
    root: &std::path::Path,
    remote: &Option<String>,
    action: GatewayAction,
    stopped: &mut bool,
) -> Result<Option<GatewayInfo>> {
    match action {
        GatewayAction::Status => {
            if current(shared)?.is_closed() {
                replace(shared, Client::attach(root.into(), remote.clone()))?;
            }
        }
        GatewayAction::Start => {
            if let Ok(info) = probe(current(shared)?).await {
                *stopped = false;
                return Ok(Some(info));
            }
            *stopped = false;
            replace(shared, Client::launch(root.into(), remote.clone()))?;
        }
        GatewayAction::Reconnect => replace(shared, Client::attach(root.into(), remote.clone()))?,
        GatewayAction::Stop | GatewayAction::Restart => {
            if current(shared)?.is_closed() {
                replace(shared, Client::attach(root.into(), remote.clone()))?;
            }
            let reachable = remote.is_some() && probe(current(shared)?).await.is_ok();
            if remote.is_none() || reachable || !*stopped {
                // Older gateways can acknowledge Shutdown even if they do not
                // implement the newer status reply. Never infer a PID to kill.
                stop(shared, root, remote).await?;
            }
            *stopped = true;
            if action == GatewayAction::Stop {
                return Ok(None);
            }
            *stopped = false;
            replace(shared, Client::launch(root.into(), remote.clone()))?;
        }
    }
    let info = probe(current(shared)?).await?;
    *stopped = false;
    Ok(Some(info))
}
