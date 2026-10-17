//! Gateway owns state. Trusted UI commands and untrusted agent tools are separate APIs.
use crate::{
    error::check,
    provider::{self, now},
    store::{Store, StoreError},
    transport,
};
use a206_core::*;
use a206_protocol::{Request, Response};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};
use tokio::io::AsyncWriteExt;
mod mcp_servers;
mod run;
/// Snapshot bytes a user message needs beyond its text and attachments.
const MESSAGE_OVERHEAD: usize = 4096;
mod messengers;
pub use mcp_servers::{McpOffer, McpOffers};
pub use run::{RunError, ToolError};
mod delegation;
mod skills;
pub use crate::skill_tool::SkillOffer;
pub use delegation::{Caller, Child, Delegator, DelegatorError, Reservation};

/// Streamed reply text that is not final yet. It is published as the
/// thread's assistant message at most every [`PartialReply::INTERVAL`], so a
/// fast stream does not rewrite the journal for every delta, and the newest
/// text is never left unpublished for longer than that.
#[derive(Default)]
struct PartialReply {
    message: Option<Id>,
    text: String,
    /// Text changed since it was last written to the thread.
    dirty: bool,
    published: Option<tokio::time::Instant>,
}
impl PartialReply {
    const INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
    /// Apply streamed text; `true` when it should be published now.
    fn update(&mut self, update: crate::backends::Progress) -> bool {
        match update {
            crate::backends::Progress::Text(text) => self.text = text,
            crate::backends::Progress::Delta(delta) => self.text.push_str(&delta),
            crate::backends::Progress::Activity(_) => return false,
        }
        self.dirty = true;
        self.due()
            .is_some_and(|due| due <= tokio::time::Instant::now())
    }
    /// When unpublished text should be published.
    fn due(&self) -> Option<tokio::time::Instant> {
        self.dirty.then(|| {
            self.published
                .map_or_else(tokio::time::Instant::now, |at| at + Self::INTERVAL)
        })
    }
    /// Write the text into the reply message, creating it once there is text.
    fn apply(&mut self, thread: &mut Thread) {
        self.dirty = false;
        let found = self
            .message
            .and_then(|id| thread.messages.iter_mut().find(|m| m.id == id));
        if let Some(existing) = found {
            existing.text.clone_from(&self.text);
        } else if !self.text.is_empty() {
            let id = *self.message.get_or_insert_with(Id::new_v4);
            thread.messages.push(Message {
                id,
                role: Role::Assistant,
                text: self.text.clone(),
                attachments: vec![],
                at: now(),
                backend: thread.backend,
                tool_calls: vec![],
            });
        }
    }
    /// Publish the current text as a durable thread change.
    fn publish(&mut self, store: &mut Store, id: Id) -> Result<(), GatewayError> {
        self.apply(store.thread_mut(id)?);
        store.persist(Some(EventKind::ThreadChanged { id }))?;
        self.published = Some(tokio::time::Instant::now());
        Ok(())
    }
    /// Replace streamed text with the final reply, which always gets a
    /// message; returns that message's ID.
    fn finish(mut self, thread: &mut Thread, text: String) -> Id {
        self.text = text;
        let id = *self.message.get_or_insert_with(Id::new_v4);
        if !thread.messages.iter().any(|m| m.id == id) {
            thread.messages.push(Message {
                id,
                role: Role::Assistant,
                text: String::new(),
                attachments: vec![],
                at: now(),
                backend: thread.backend,
                tool_calls: vec![],
            });
        }
        self.apply(thread);
        id
    }
}
/// Why a gateway command failed. Validation reasons are static so clients can
/// match the variant; every wrapped failure keeps its cause in `source()`.
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    /// The command failed validation.
    #[error("{0}")]
    Invalid(&'static str),
    /// A referenced thread, project, provider or transfer does not exist.
    #[error("{0}")]
    NotFound(&'static str),
    /// The command conflicts with current state; refresh and retry.
    #[error("{0}")]
    Conflict(&'static str),
    /// Policy or the thread's grants forbid the command.
    #[error("{0}")]
    Denied(&'static str),
    #[error("gateway is stopping; start it again before submitting work")]
    Draining,
    #[error(
        "this preview reached its history capacity; export the state directory before continuing in a new profile"
    )]
    HistoryFull,
    #[error("{lock} lock poisoned")]
    Poisoned { lock: &'static str },
    #[error("operation completed but its receipt could not be persisted")]
    Receipt(#[source] StoreError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Joshua(#[from] crate::joshua::JoshuaError),
    #[error(transparent)]
    Provider(#[from] provider::ProviderError),
    #[error(transparent)]
    Endpoint(#[from] provider::EndpointError),
    #[error(transparent)]
    Presets(#[from] provider::PresetError),
    #[error(transparent)]
    Backend(#[from] crate::backends::BackendError),
    #[error(transparent)]
    Process(#[from] crate::process::ProcessError),
    #[error(transparent)]
    Download(#[from] crate::network::DownloadError),
    #[error(transparent)]
    Extension(#[from] crate::extension_host::ExtensionError),
    #[error(transparent)]
    Mcp(#[from] a206_mcp::McpError),
    #[error(transparent)]
    Skill(#[from] a206_skills::SkillError),
    #[error(transparent)]
    Prompt(#[from] crate::prompt_tool::PromptError),
    #[error(transparent)]
    Frame(#[from] transport::FrameError),
    #[error(transparent)]
    Run(#[from] run::RunError),
    #[error("state could not be encoded")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("settings could not be encoded")]
    EncodeSettings(#[source] toml::ser::Error),
    #[error("could not write settings.toml")]
    WriteSettings(#[source] std::io::Error),
    #[error("could not record the stop request")]
    MarkStopped(#[source] std::io::Error),
    #[error("gateway cleanup timed out; service remains running")]
    ShutdownTimeout(#[source] tokio::time::error::Elapsed),
    #[error("workspace is missing or unreadable")]
    Workspace(#[source] std::io::Error),
    #[error("document is missing or outside the project")]
    Document(#[source] std::io::Error),
    #[error("this viewer supports UTF-8 text and Markdown")]
    NotText(#[source] std::io::Error),
    #[error("invalid download URL")]
    DownloadUrl(#[source] url::ParseError),
    #[error("could not prepare the download directory")]
    DownloadDirectory(#[source] std::io::Error),
    #[error("could not write the partial download")]
    Partial(#[source] std::io::Error),
    #[error("could not publish the download")]
    Publish(#[source] std::io::Error),
    #[error("download cancelled")]
    DownloadCancelled,
    #[error("download integrity check failed")]
    Integrity,
    #[error("download length mismatch: expected {expected} bytes, received {received}")]
    Length { expected: u64, received: u64 },
    #[error("gateway task failed")]
    Task(#[from] tokio::task::JoinError),
    #[error("gateway request limiter closed")]
    Limiter(#[source] tokio::sync::AcquireError),
    #[error("reply channel closed")]
    ReplyChannel,
}
impl crate::error::Classified for GatewayError {
    fn kind(&self) -> Option<ErrorKind> {
        use crate::error::classify;
        Some(match self {
            Self::Invalid(_) | Self::DownloadUrl(_) => ErrorKind::InvalidRequest,
            Self::NotFound(_) => ErrorKind::NotFound,
            Self::Conflict(_) => ErrorKind::Conflict,
            Self::Denied(_) => ErrorKind::PermissionDenied,
            Self::Draining | Self::ReplyChannel => ErrorKind::Unavailable,
            Self::HistoryFull
            | Self::WriteSettings(_)
            | Self::MarkStopped(_)
            | Self::DownloadDirectory(_)
            | Self::Partial(_) => ErrorKind::Storage,
            Self::Poisoned { .. }
            | Self::Encode(_)
            | Self::EncodeSettings(_)
            | Self::Task(_)
            | Self::Limiter(_) => ErrorKind::Internal,
            Self::ShutdownTimeout(_) => ErrorKind::Timeout,
            Self::DownloadCancelled => ErrorKind::Cancelled,
            Self::Integrity | Self::Length { .. } => ErrorKind::Provider,
            // `transparent` hides these from the chain, so classify them here.
            Self::Store(error) => return classify(error),
            Self::Joshua(error) => return classify(error),
            Self::Provider(error) => return classify(error),
            Self::Endpoint(error) => return classify(error),
            Self::Presets(error) => return classify(error),
            Self::Backend(error) => return classify(error),
            Self::Process(error) => return classify(error),
            Self::Download(error) => return classify(error),
            Self::Extension(error) => return classify(error),
            Self::Mcp(error) => return classify(error),
            Self::Skill(error) => return classify(error),
            Self::Prompt(error) => return classify(error),
            Self::Frame(error) => return classify(error),
            Self::Run(error) => return classify(error),
            // The cause's own kind (missing, denied, invalid data) is the useful one.
            Self::Receipt(_)
            | Self::Workspace(_)
            | Self::Document(_)
            | Self::NotText(_)
            | Self::Publish(_) => return None,
        })
    }
}
type Result<T, E = GatewayError> = std::result::Result<T, E>;
fn poisoned<T>(lock: &'static str) -> impl FnOnce(T) -> GatewayError {
    move |_| GatewayError::Poisoned { lock }
}
#[derive(Default)]
struct Admission {
    draining: bool,
    active: usize,
}
struct ActiveRequest(Arc<Mutex<Admission>>);
impl Drop for ActiveRequest {
    fn drop(&mut self) {
        if let Ok(mut admission) = self.0.lock() {
            admission.active -= 1;
        }
    }
}
struct DrainAttempt {
    admission: Arc<Mutex<Admission>>,
    committed: bool,
}
/// Dropping a freshly committed acknowledgement also completes shutdown when
/// its client disconnects or the writer cannot deliver earlier queued replies.
struct StopAfterReply(Option<tokio_util::sync::CancellationToken>);
impl Drop for StopAfterReply {
    fn drop(&mut self) {
        if let Some(stop) = &self.0 {
            stop.cancel();
        }
    }
}
impl DrainAttempt {
    fn begin(admission: &Arc<Mutex<Admission>>) -> Result<Self> {
        let mut state = admission.lock().map_err(poisoned("gateway admission"))?;
        check(
            !state.draining,
            GatewayError::Conflict("gateway shutdown is already complete"),
        )?;
        state.draining = true;
        Ok(Self {
            admission: admission.clone(),
            committed: false,
        })
    }
}
impl Drop for DrainAttempt {
    fn drop(&mut self) {
        if !self.committed
            && let Ok(mut admission) = self.admission.lock()
        {
            admission.draining = false;
        }
    }
}
#[derive(Clone)]
pub struct Gateway {
    admission: Arc<Mutex<Admission>>,
    shutdown_committed: Arc<tokio::sync::Mutex<bool>>,
    started: std::time::Instant,
    stop: tokio_util::sync::CancellationToken,
    joshua: Arc<tokio::sync::Mutex<crate::joshua::Manager>>,
    store: Arc<Mutex<Store>>,
    extensions: crate::extension_host::ExtensionHost,
    downloads: Arc<Mutex<BTreeMap<Id, tokio_util::sync::CancellationToken>>>,
    running: Arc<Mutex<BTreeMap<Id, tokio_util::sync::CancellationToken>>>,
    /// Held only in memory: a restart needs a fresh pairing from the user.
    pairing: Arc<Mutex<Option<Paired>>>,
    /// Orders PairBroker commands, so a slow older verification never
    /// replaces a newer pairing.
    pair_requests: Arc<std::sync::atomic::AtomicU64>,
    /// Last automatic catalog attempt per provider, reset by a new pairing.
    refreshed: Arc<Mutex<BTreeMap<Id, u64>>>,
    paired: Arc<tokio::sync::Notify>,
    tool_hosts: Arc<Vec<Arc<dyn crate::tools::ToolHost>>>,
    /// Runs waiting for the user, by approval ID. Only `ResolveApproval` answers them.
    approvals: Arc<Mutex<BTreeMap<Id, run::Waiter>>>,
    /// Questions `ask_user` calls are waiting on. Only `AnswerPrompt` answers them.
    prompts: Arc<crate::prompt_tool::Prompts>,
    /// Bumped whenever a tool permission, grant or mode changes, so running
    /// tool calls and approval waits re-check their authority at once.
    policy: Arc<tokio::sync::watch::Sender<u64>>,
    /// Counts pairings, so messengers waiting for the vault wake on a new one.
    pairings: tokio::sync::watch::Sender<u64>,
    /// What the messenger supervisor should run.
    messengers: tokio::sync::watch::Sender<Vec<crate::messengers::Desired>>,
    /// Messengers the user disconnected; they connect again only on request.
    messengers_stopped: Arc<Mutex<BTreeSet<String>>>,
    mcp: Arc<mcp_servers::McpServers>,
    /// This gateway's subagent tools and the hosts their subagents run with.
    delegation: delegation::Binding,
    /// Runs the subagent tools reserved before starting them, by subagent.
    reserved: delegation::Reserved,
}
/// The broker pairing this gateway holds, with the connection that anchors it.
struct Paired {
    pairing: BrokerPairing,
    _anchor: provider::PairingAnchor,
    order: u64,
}
/// Minimum seconds between automatic refreshes of one stale catalog.
const REFRESH_RETRY: u64 = 300;
/// What the request journal keeps of a command: an installed module is
/// recorded by its SHA-256 rather than copied, which still tells a retry
/// apart from a different request reusing its id.
fn journal_form(command: &Command) -> std::borrow::Cow<'_, Command> {
    match command {
        Command::InstallExtension { manifest, module } => {
            std::borrow::Cow::Owned(Command::InstallExtension {
                manifest: manifest.clone(),
                module: Sha256::digest(module).to_vec(),
            })
        }
        command => std::borrow::Cow::Borrowed(command),
    }
}

impl Gateway {
    pub fn open(root: &Path) -> Result<Self, StoreError> {
        let mut store = Store::open(root)?;
        // Messenger state is runtime state: nothing is connected yet.
        store.state.messengers =
            Self::messenger_statuses(&store.state.messengers, &store.state.settings.messengers)
                .into_iter()
                .map(|status| MessengerStatus {
                    state: MessengerState::Stopped,
                    account: None,
                    failure: None,
                    ..status
                })
                .collect();
        // Nobody has disconnected anything yet, so enabled entries run.
        let desired = store
            .state
            .settings
            .messengers
            .iter()
            .map(|config| crate::messengers::Desired {
                run: config.enabled,
                config: config.clone(),
            })
            .collect();
        let store = Arc::new(Mutex::new(store));
        let extensions = crate::extension_host::ExtensionHost::new(store.clone(), root.to_owned());
        // Enabled, granted extension tools are offered by default; each
        // conversation still denies them until the user permits one.
        let extension_tools: Arc<dyn crate::tools::ToolHost> = Arc::new(
            crate::extension_tools::ExtensionToolHost::new(extensions.clone()),
        );
        // Proposing grants nothing; installing a proposal is a user operation.
        let propose_tools: Arc<dyn crate::tools::ToolHost> =
            Arc::new(crate::propose_tool::ProposeHost::new(extensions.clone()));
        let prompts = Arc::new(crate::prompt_tool::Prompts::new(store.clone()));
        // Approved skills; each conversation still denies the tools until the
        // user permits them.
        let skill_tools: Arc<dyn crate::tools::ToolHost> =
            Arc::new(crate::skill_tool::SkillToolHost::new(store.clone(), root));
        let mut gateway = Self {
            admission: Arc::new(Mutex::new(Admission::default())),
            shutdown_committed: Arc::new(tokio::sync::Mutex::new(false)),
            started: std::time::Instant::now(),
            stop: tokio_util::sync::CancellationToken::new(),
            joshua: Arc::new(tokio::sync::Mutex::new(crate::joshua::Manager::default())),
            extensions,
            store,
            downloads: Arc::new(Mutex::new(BTreeMap::new())),
            running: Arc::new(Mutex::new(BTreeMap::new())),
            pairing: Arc::new(Mutex::new(None)),
            pair_requests: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            refreshed: Arc::new(Mutex::new(BTreeMap::new())),
            paired: Arc::new(tokio::sync::Notify::new()),
            tool_hosts: Arc::new(vec![
                extension_tools,
                Arc::new(crate::exec_tool::ExecHost::new(root)),
                Arc::new(crate::prompt_tool::PromptHost::new(prompts.clone())),
                skill_tools,
                propose_tools,
            ]),
            approvals: Arc::new(Mutex::new(BTreeMap::new())),
            prompts,
            policy: Arc::new(tokio::sync::watch::Sender::new(0)),
            pairings: tokio::sync::watch::Sender::new(0),
            messengers: tokio::sync::watch::Sender::new(desired),
            messengers_stopped: Arc::new(Mutex::new(BTreeSet::new())),
            mcp: Arc::default(),
            delegation: Default::default(),
            reserved: Arc::default(),
        };
        // Subagent tools; each conversation still denies them until the user
        // permits them, and they are offered only with subagent models.
        gateway.bind_delegation();
        Ok(gateway)
    }
    fn pairing(&self) -> Result<Option<BrokerPairing>> {
        Ok(self
            .pairing
            .lock()
            .map_err(poisoned("pairing"))?
            .as_ref()
            .map(|paired| paired.pairing.clone()))
    }
    fn lock(&self) -> Result<MutexGuard<'_, Store>> {
        self.store.lock().map_err(poisoned("state"))
    }
    /// Wakes the automatic catalog refresh when a new broker pairing arrives.
    pub(crate) fn paired_signal(&self) -> Arc<tokio::sync::Notify> {
        self.paired.clone()
    }
    /// Refresh each stale catalog not attempted within `REFRESH_RETRY`.
    /// Failed refreshes keep the expired cache and are recorded, not returned.
    pub async fn refresh_stale_catalogs(&self) -> Result<()> {
        let snapshot = self.snapshot()?;
        let now = provider::now();
        for config in snapshot.settings.providers {
            let fresh = snapshot
                .catalogs
                .iter()
                .any(|c| c.provider == config.id && !c.stale(now));
            let due = {
                let mut refreshed = self.refreshed.lock().map_err(poisoned("catalog refresh"))?;
                let due = !fresh
                    && refreshed
                        .get(&config.id)
                        .is_none_or(|last| now.saturating_sub(*last) >= REFRESH_RETRY);
                if due {
                    refreshed.insert(config.id, now);
                }
                due
            };
            if due {
                let reply = self
                    .request(Request {
                        id: Id::new_v4(),
                        body: Command::RefreshCatalog {
                            provider: config.id,
                        },
                    })
                    .await;
                if let Reply::Error(report) = reply.body {
                    let _provider = tracing::warn_span!("catalog", provider = %config.id).entered();
                    crate::error::record("refresh_catalog", &report);
                }
            }
        }
        Ok(())
    }
    pub fn stop_signal(&self) -> tokio_util::sync::CancellationToken {
        self.stop.clone()
    }
    pub fn snapshot(&self) -> Result<Snapshot> {
        Ok(self.lock()?.state.clone())
    }
    fn clear_managed_joshua(&self) -> Result<()> {
        let mut store = self.lock()?;
        let Some(provider) = store.state.managed_joshua_provider else {
            return Ok(());
        };
        let running = self.running.lock().map_err(poisoned("run"))?;
        for thread in &store.state.threads {
            if thread
                .model
                .as_ref()
                .is_some_and(|key| key.provider == provider)
                && let Some(token) = running.get(&thread.id)
            {
                token.cancel();
            }
        }
        store.state.clear_managed_joshua();
        store.persist(Some(EventKind::SettingsChanged))?;
        Ok(())
    }
    pub async fn request(&self, request: Request<Command>) -> Response<Reply> {
        self.request_with_effect(request).await.0
    }
    /// The stop effect belongs only to fresh execution, never a durable replay.
    async fn request_with_effect(&self, request: Request<Command>) -> (Response<Reply>, bool) {
        let id = request.id;
        let operation = request.body.name();
        let span = tracing::info_span!("request", %id, operation);
        let (body, stop) = tracing::Instrument::instrument(self.execute(request), span).await;
        let body = match body {
            Ok(reply) => reply,
            Err(error) => Reply::Error(crate::error::correlated(operation, id, &error)),
        };
        (Response { id, body }, stop)
    }
    async fn execute(&self, request: Request<Command>) -> (Result<Reply>, bool) {
        let id = request.id;
        let operation = request.body.name();
        let is_shutdown = matches!(request.body, Command::Shutdown);
        let _active = if !matches!(
            request.body,
            Command::Snapshot
                | Command::GatewayStatus
                | Command::Shutdown
                | Command::Events { .. }
                | Command::PairBroker { .. }
        ) {
            let Ok(mut admission) = self.admission.lock() else {
                return (
                    Err(GatewayError::Poisoned {
                        lock: "gateway admission",
                    }),
                    false,
                );
            };
            if admission.draining {
                return (Err(GatewayError::Draining), false);
            }
            admission.active += 1;
            Some(ActiveRequest(self.admission.clone()))
        } else {
            None
        };
        let mutation = !matches!(
            request.body,
            Command::Snapshot
                | Command::GatewayStatus
                | Command::JoshuaStatus
                | Command::Events { .. }
                | Command::GitStatus { .. }
                | Command::ReadDocument { .. }
                // Extension output can carry workspace content, so it is
                // never persisted in the request journal.
                | Command::OpenPanel { .. }
                | Command::CallTool { .. }
                | Command::Skills
                | Command::ReadSkill { .. }
                | Command::ListTools { .. }
                // Never journal a pairing: request bodies are persisted.
                | Command::PairBroker { .. }
        );
        let begin = if mutation {
            self.lock()
                .and_then(|mut store| Ok(store.begin(id, &journal_form(&request.body))?))
        } else {
            Ok(None)
        };
        match begin {
            Ok(Some(reply)) => (Ok(reply), false),
            Err(error) => (Err(error), false),
            Ok(None) => {
                // A failed command is a reply: persist its report as the receipt.
                let reply = match self.handle(request.body).await {
                    Ok(reply) => reply,
                    Err(error) => Reply::Error(crate::error::correlated(operation, id, &error)),
                };
                if mutation {
                    // In-flight MCP calls re-check their authorization.
                    self.mcp.state_changed();
                }
                let stop = is_shutdown && matches!(reply, Reply::Ok);
                if mutation
                    && let Err(error) = self
                        .lock()
                        .and_then(|s| s.finish(id, &reply).map_err(GatewayError::Receipt))
                {
                    return (Err(error), stop);
                }
                (Ok(reply), stop)
            }
        }
    }
    async fn handle(&self, command: Command) -> Result<Reply> {
        match command {
            Command::GatewayStatus => Ok(Reply::Gateway(GatewayInfo {
                pid: std::process::id(),
                version: env!("CARGO_PKG_VERSION").into(),
                protocol: a206_protocol::VERSION,
                uptime_seconds: self.started.elapsed().as_secs(),
                data_dir: self.lock()?.root.display().to_string(),
                active_runs: self.running.lock().map_err(poisoned("run"))?.len(),
                active_downloads: self.downloads.lock().map_err(poisoned("download"))?.len(),
                accepting_work: Some(
                    !self
                        .admission
                        .lock()
                        .map_err(poisoned("gateway admission"))?
                        .draining,
                ),
                vault_paired: Some(self.pairing()?.is_some()),
            })),
            Command::Snapshot => Ok(Reply::Snapshot(Box::new(self.snapshot()?))),
            Command::PairBroker { pairing } => {
                let order = self
                    .pair_requests
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // A retried delivery must keep the anchor the broker voids the
                // pairing without, but is still checked against the broker.
                let retry = self
                    .pairing
                    .lock()
                    .map_err(poisoned("pairing"))?
                    .as_ref()
                    .is_some_and(|paired| paired.pairing == pairing);
                let root = self.lock()?.root.clone();
                let verified = provider::verify_pairing(&root, &pairing).await;
                if retry {
                    // The extra connection is not the anchor, so dropping it
                    // leaves the pairing intact.
                    // A failed check is reported, but only the broker voids a
                    // pairing: a transient failure must not drop a good anchor.
                    verified?;
                    return Ok(Reply::Ok);
                }
                let anchor = verified?;
                {
                    let mut current = self.pairing.lock().map_err(poisoned("pairing"))?;
                    if current.as_ref().is_some_and(|paired| paired.order > order) {
                        // A newer pairing arrived meanwhile; this one is superseded.
                        return Ok(Reply::Ok);
                    }
                    // Replacing the old anchor closes it; its pairing is already superseded.
                    *current = Some(Paired {
                        pairing,
                        _anchor: anchor,
                        order,
                    });
                }
                // Discovery that failed for want of a pairing may retry now.
                self.refreshed
                    .lock()
                    .map_err(poisoned("catalog refresh"))?
                    .clear();
                self.paired.notify_one();
                self.pairings
                    .send_modify(|count| *count = count.wrapping_add(1));
                Ok(Reply::Ok)
            }
            Command::JoshuaStatus => {
                let mut manager = self.joshua.lock().await;
                let status = manager.status()?;
                if !manager.is_running() {
                    self.clear_managed_joshua()?;
                }
                Ok(Reply::Text(status))
            }
            Command::StartJoshua {
                label,
                model_file,
                port,
                remote,
            } => {
                let root = self.lock()?.root.clone();
                let mut manager = self.joshua.lock().await;
                manager.status()?;
                if !manager.is_running() {
                    self.clear_managed_joshua()?;
                }
                let provider = manager
                    .start(label, model_file, port, remote, &root)
                    .await?;
                let id = provider.id;
                let persisted = (|| -> Result<()> {
                    let mut store = self.lock()?;
                    store.state.settings.providers.push(provider);
                    store.state.managed_joshua_provider = Some(id);
                    store.state.settings.revision += 1;
                    store.persist(Some(EventKind::SettingsChanged))?;
                    Ok(())
                })();
                if let Err(error) = persisted {
                    // The persistence failure is the one reported; a failed
                    // rollback is recorded rather than replacing it.
                    if let Err(stop) = manager.stop().await {
                        crate::error::handle("stop_joshua", &stop);
                    }
                    return Err(error);
                }
                Ok(Reply::Text(format!(
                    "Joshua started. Refresh the new connection to discover its models. Provider: {id}"
                )))
            }
            Command::StopJoshua => {
                let mut manager = self.joshua.lock().await;
                manager.stop().await?;
                self.clear_managed_joshua()?;
                Ok(Reply::Ok)
            }
            Command::Events { after } => Ok(Reply::Events(self.lock()?.events(after)?)),
            Command::SaveSettings(mut settings) => {
                provider::validate_presets(&settings.provider_presets)?;
                messengers::validate(&settings.messengers)?;
                let mut backend_kinds = BTreeSet::new();
                for backend in &settings.backends {
                    crate::backends::validate_config(backend)?;
                    check(
                        backend_kinds.insert(backend.kind.label()),
                        GatewayError::Invalid("duplicate coding backend configuration"),
                    )?;
                }
                let mut ids = BTreeSet::new();
                for config in &settings.providers {
                    check(
                        ids.insert(config.id),
                        GatewayError::Invalid("duplicate provider id"),
                    )?;
                    provider::endpoint(config, "models")?;
                    check(
                        !config.label.trim().is_empty(),
                        GatewayError::Invalid("provider label is required"),
                    )?;
                }
                for mode in Mode::ALL {
                    check(
                        settings.prompts.iter().filter(|p| p.mode == mode).count() == 1,
                        GatewayError::Invalid("each mode needs exactly one prompt"),
                    )?;
                }
                let mut store = self.lock()?;
                check(
                    settings.revision == store.state.settings.revision,
                    GatewayError::Conflict(
                        "settings changed in another operation; refresh and retry your change",
                    ),
                )?;
                // Skill approval is its own user operation, never a settings edit.
                settings.enabled_skills = store.state.settings.enabled_skills.clone();
                let mcp_servers = Self::mcp_statuses(&store.state, &settings.mcp_servers)?;
                if let Some(id) = store.state.managed_joshua_provider {
                    let owned = store.state.settings.providers.iter().find(|provider| provider.id == id)
                        .ok_or(GatewayError::Conflict("managed Joshua connection is missing; stop Joshua before applying settings"))?;
                    check(
                        settings
                            .providers
                            .iter()
                            .find(|provider| provider.id == id)
                            .is_some_and(|provider| {
                                provider.base_url == owned.base_url
                                    && provider.api == owned.api
                                    && provider.credential == owned.credential
                            }),
                        GatewayError::Conflict(
                            "stop managed Joshua before removing its connection or changing its endpoint, protocol or credentials",
                        ),
                    )?;
                }
                // Revocation also stops already-running inference and coding backends.
                let running = self.running.lock().map_err(poisoned("run"))?;
                for thread in &store.state.threads {
                    let provider_changed = thread.model.as_ref().is_some_and(|key| {
                        let previous = store
                            .state
                            .settings
                            .providers
                            .iter()
                            .find(|p| p.id == key.provider);
                        settings.providers.iter().find(|p| p.id == key.provider) != previous
                    });
                    let backend_changed = thread.backend != CodingBackend::Native
                        && settings.backends.iter().find(|b| b.kind == thread.backend)
                            != store
                                .state
                                .settings
                                .backends
                                .iter()
                                .find(|b| b.kind == thread.backend);
                    if (provider_changed || backend_changed)
                        && let Some(token) = running.get(&thread.id)
                    {
                        token.cancel();
                    }
                }
                drop(running);
                settings.revision += 1;
                write_settings(&store.root, &settings)?;
                // Endpoint/credential changes invalidate any facts from the previous source.
                let old = store.state.settings.providers.clone();
                store.state.catalogs.retain(|c| {
                    settings
                        .providers
                        .iter()
                        .any(|p| p.id == c.provider && old.contains(p))
                });
                store.state.messengers =
                    Self::messenger_statuses(&store.state.messengers, &settings.messengers);
                let previous = std::mem::replace(&mut store.state.settings, settings);
                store.state.mcp_servers = mcp_servers;
                store.persist(Some(EventKind::SettingsChanged))?;
                self.stop_changed_mcp(&previous.mcp_servers, &store.state.settings.mcp_servers)?;
                drop(store);
                self.publish_messengers()?;
                Ok(Reply::Ok)
            }
            Command::ConnectMessenger { name } => self.connect_messenger(&name, true),
            Command::DisconnectMessenger { name } => self.connect_messenger(&name, false),
            Command::RefreshCatalog { provider } => {
                let config = self
                    .lock()?
                    .state
                    .settings
                    .providers
                    .iter()
                    .find(|p| p.id == provider)
                    .cloned()
                    .ok_or(GatewayError::NotFound("provider not configured"))?;
                let root = self.lock()?.root.clone();
                let pairing = self.pairing()?;
                let broker = provider::Broker {
                    root: &root,
                    pairing: pairing.as_ref(),
                };
                let catalog = provider::catalog(&config, broker).await?;
                let mut store = self.lock()?;
                check(
                    store.state.settings.providers.contains(&config),
                    GatewayError::Conflict("provider configuration changed during refresh"),
                )?;
                store.state.catalogs.retain(|c| c.provider != provider);
                store.state.catalogs.push(catalog.clone());
                store.persist(Some(EventKind::CatalogChanged { provider }))?;
                Ok(Reply::Catalog(catalog))
            }
            Command::CreateThread { title, mode } => {
                check(
                    !title.trim().is_empty() && title.len() <= 512,
                    GatewayError::Invalid("thread title must be 1–512 bytes"),
                )?;
                let thread = Thread::new(title, mode);
                let mut store = self.lock()?;
                store.state.threads.push(thread.clone());
                store.persist(Some(EventKind::ThreadChanged { id: thread.id }))?;
                Ok(Reply::Thread(Box::new(thread)))
            }
            Command::SetSubagentModel { id, model, allowed } => {
                let mut store = self.lock()?;
                if allowed {
                    validate_model(&store.state, &model)?;
                }
                let thread = store.thread_mut(id)?;
                if allowed {
                    thread.subagent_models.insert(model.clone());
                    thread.grant.models.insert(model.clone());
                } else {
                    thread.subagent_models.remove(&model);
                    if thread.model.as_ref() != Some(&model) {
                        thread.grant.models.remove(&model);
                    }
                }
                if !allowed {
                    let running = self.running.lock().map_err(poisoned("run"))?;
                    for child in store.state.threads.iter().filter(|child| {
                        child.parent == Some(id) && child.model.as_ref() == Some(&model)
                    }) {
                        if let Some(token) = running.get(&child.id) {
                            token.cancel();
                        }
                    }
                }
                store.persist(Some(EventKind::ThreadChanged { id }))?;
                Ok(Reply::Ok)
            }
            Command::RenameThread { id, title } => {
                check(
                    !title.trim().is_empty() && title.len() <= 512,
                    GatewayError::Invalid("thread title must be 1–512 bytes"),
                )?;
                let mut store = self.lock()?;
                store.thread_mut(id)?.title = title;
                store.persist(Some(EventKind::ThreadChanged { id }))?;
                Ok(Reply::Ok)
            }
            Command::ConfigureThread {
                id,
                mode,
                model,
                subagent_models,
            } => {
                let mut store = self.lock()?;
                for key in model.iter().chain(&subagent_models) {
                    validate_model(&store.state, key)?;
                }
                let thread = store.thread_mut(id)?;
                check(
                    thread.state != RunState::Running,
                    GatewayError::Conflict(
                        "wait for the current response before changing its configuration",
                    ),
                )?;
                thread.mode = mode;
                if thread.model != model {
                    thread.effort = None;
                    thread.usage = None;
                    thread.run_usage = None;
                }
                thread.model = model;
                thread.subagent_models = subagent_models;
                thread.grant.models = thread.subagent_models.clone();
                if let Some(model) = &thread.model {
                    thread.grant.models.insert(model.clone());
                }
                thread.grant.capabilities = thread
                    .grant
                    .capabilities
                    .intersection(&mode.ceiling())
                    .copied()
                    .collect();
                let thread = thread.clone();
                store.persist(Some(EventKind::ThreadChanged { id }))?;
                self.policy_changed();
                Ok(Reply::Thread(Box::new(thread)))
            }
            Command::SetEffort { thread: id, effort } => {
                let mut store = self.lock()?;
                let thread = store.thread(id)?;
                check(
                    thread.state != RunState::Running,
                    GatewayError::Conflict("wait for the response before changing effort"),
                )?;
                validate_effort(&store.state, thread.model.as_ref(), effort.as_deref())?;
                store.thread_mut(id)?.effort = effort;
                store.persist(Some(EventKind::ThreadChanged { id }))?;
                Ok(Reply::Ok)
            }
            Command::SetBackend {
                thread: id,
                backend,
                capabilities,
            } => {
                let mut store = self.lock()?;
                let thread = store.thread(id)?;
                check(
                    thread.state != RunState::Running,
                    GatewayError::Conflict("stop the run before changing backend permissions"),
                )?;
                check(
                    thread.parent.is_none(),
                    GatewayError::Denied("delegated threads cannot change backend grants"),
                )?;
                let supported: BTreeSet<_> = [
                    Capability::ReadWorkspace,
                    Capability::WriteWorkspace,
                    Capability::Execute,
                ]
                .into();
                check(
                    capabilities.is_subset(&supported)
                        && capabilities.is_subset(&thread.mode.ceiling()),
                    GatewayError::Denied("backend capability is unavailable in this mode"),
                )?;
                check(
                    !capabilities.contains(&Capability::WriteWorkspace)
                        || capabilities.contains(&Capability::ReadWorkspace),
                    GatewayError::Invalid("write access also requires read access"),
                )?;
                check(
                    capabilities.is_empty() || thread.project.is_some(),
                    GatewayError::Invalid("associate a project before granting workspace tools"),
                )?;
                if backend != CodingBackend::Native {
                    check(
                        store
                            .state
                            .settings
                            .backends
                            .iter()
                            .any(|b| b.kind == backend),
                        GatewayError::Invalid("configure the coding backend in Connections first"),
                    )?;
                }
                let thread = store.thread_mut(id)?;
                thread.backend = backend;
                thread.backend_activity = None;
                thread.grant.capabilities = capabilities;
                store.persist(Some(EventKind::ThreadChanged { id }))?;
                self.policy_changed();
                Ok(Reply::Ok)
            }
            Command::Send {
                thread: id,
                text,
                attachments,
            } => {
                check(
                    attachments.len() <= 8,
                    GatewayError::Invalid("attach at most eight text documents"),
                )?;
                let attachment_size: usize = attachments.iter().map(|a| a.text.len()).sum();
                check(
                    text.len() + attachment_size <= 128 * 1024,
                    GatewayError::Invalid("message and attachments exceed 128 KiB"),
                )?;
                for attachment in &attachments {
                    check(
                        !attachment.name.is_empty()
                            && attachment.name.len() <= 255
                            && !attachment.name.chars().any(char::is_control),
                        GatewayError::Invalid("invalid attachment name"),
                    )?;
                    check(
                        !attachment.text.contains('\0'),
                        GatewayError::Invalid("only UTF-8 text attachments are supported"),
                    )?;
                }
                check(
                    (!text.trim().is_empty() || !attachments.is_empty())
                        && text.len() <= 128 * 1024,
                    GatewayError::Invalid("message must be 1–131072 bytes"),
                )?;
                let (token, config, thread, prompt, info, backend, workspace, root) = {
                    let mut store = self.lock()?;
                    // Admit the message only with room for it and for the
                    // run's first round, which applies the same bound, so an
                    // accepted message is never left without a reply for lack
                    // of storage.
                    check(
                        store.remaining()?
                            >= text.len() + attachment_size + MESSAGE_OVERHEAD + run::NEXT_ROUND,
                        GatewayError::HistoryFull,
                    )?;
                    let current = store.thread(id)?;
                    check(
                        current.state != RunState::Running,
                        GatewayError::Conflict("thread is already running"),
                    )?;
                    let key = current.model.as_ref().ok_or(GatewayError::Invalid(
                        "connect a provider and select a model first",
                    ))?;
                    validate_model(&store.state, key)?;
                    validate_effort(&store.state, Some(key), current.effort.as_deref())?;
                    let info = store
                        .state
                        .catalogs
                        .iter()
                        .flat_map(|c| &c.models)
                        .find(|m| &m.key == key)
                        .cloned()
                        .ok_or(GatewayError::Conflict("model metadata missing"))?;
                    if let Some(parent) = current.parent {
                        check(
                            store.thread(parent)?.subagent_models.contains(key),
                            GatewayError::Denied("parent revoked this subagent model"),
                        )?;
                    }
                    let config = store
                        .state
                        .settings
                        .providers
                        .iter()
                        .find(|p| p.id == key.provider)
                        .cloned()
                        .ok_or(GatewayError::NotFound("provider not configured"))?;
                    let prompt = store
                        .state
                        .settings
                        .prompts
                        .iter()
                        .find(|p| p.mode == current.mode)
                        .map(|p| p.prompt.clone())
                        .ok_or(GatewayError::Invalid("mode prompt missing"))?;
                    let backend = if current.backend == CodingBackend::Native {
                        None
                    } else {
                        let backend = store
                            .state
                            .settings
                            .backends
                            .iter()
                            .find(|b| b.kind == current.backend)
                            .cloned()
                            .ok_or(GatewayError::Invalid("coding backend is not configured"))?;
                        crate::backends::validate_selection(&config, current, &backend)?;
                        Some(backend)
                    };
                    let workspace = current
                        .project
                        .and_then(|id| store.state.projects.iter().find(|p| p.id == id))
                        .map(|p| PathBuf::from(&p.root));
                    let root = store.root.clone();
                    let thread = store.thread_mut(id)?;
                    thread.messages.push(Message {
                        id: Id::new_v4(),
                        role: Role::User,
                        text,
                        attachments,
                        at: now(),
                        backend: thread.backend,
                        tool_calls: vec![],
                    });
                    thread.state = RunState::Running;
                    thread.backend_activity = None;
                    thread.error = None;
                    // The previous run's cost is not this run's.
                    thread.run_usage = None;
                    let copy = thread.clone();
                    // A subagent run its parent reserved is cancelled with it.
                    let token = delegation::admit(self, id)?;
                    store.persist(Some(EventKind::ThreadChanged { id }))?;
                    (token, config, copy, prompt, info, backend, workspace, root)
                };
                let Some(backend) = backend else {
                    let ended = self
                        .native_run(&run::Native {
                            thread: id,
                            config,
                            info,
                            prompt,
                            root,
                            cancel: token.clone(),
                        })
                        .await;
                    self.running.lock().map_err(poisoned("run"))?.remove(&id);
                    self.expire_approvals(id)?;
                    let mut store = self.lock()?;
                    let thread = store.thread_mut(id)?;
                    run::settle_unfinished(thread);
                    match ended {
                        run::Ended::Complete { last, total } => {
                            thread.usage = Some(last);
                            thread.run_usage = Some(total);
                            thread.state = RunState::Complete;
                        }
                        run::Ended::Failed(error) => fail_run(thread, id, &error),
                        run::Ended::Cancelled => thread.state = RunState::Cancelled,
                    }
                    let thread = thread.clone();
                    store.persist(Some(EventKind::ThreadChanged { id }))?;
                    return Ok(Reply::Thread(Box::new(thread)));
                };
                let (sender, mut progress) = tokio::sync::mpsc::channel(32);
                let operation = async {
                    Ok::<_, GatewayError>(
                        crate::backends::complete(crate::backends::Run {
                            config: &config,
                            thread: &thread,
                            prompt: &prompt,
                            info: &info,
                            backend: &backend,
                            workspace: workspace.as_deref(),
                            state_root: &root,
                            progress: sender,
                            cancel: token.clone(),
                        })
                        .await?,
                    )
                };
                let mut operation = Box::pin(operation);
                let mut progress_open = true;
                let mut partial = PartialReply::default();
                let mut activity_count = 0;
                let result = loop {
                    let due = partial.due();
                    tokio::select! {
                        result = &mut operation => break (!token.is_cancelled()).then_some(result),
                        _ = tokio::time::sleep_until(due.unwrap_or_else(tokio::time::Instant::now)), if due.is_some() => {
                            let mut store = self.lock()?;
                            partial.publish(&mut store, id)?;
                        }
                        update = progress.recv(), if progress_open => {
                            let Some(update) = update else { progress_open = false; continue; };
                            if let crate::backends::Progress::Activity(title) = update {
                                if activity_count < 128 {
                                    activity_count += 1;
                                    let mut store = self.lock()?;
                                    store.thread_mut(id)?.backend_activity = Some(title.clone());
                                    store.persist(Some(EventKind::BackendActivity { thread: id, title }))?;
                                }
                            } else if partial.update(update) {
                                let mut store = self.lock()?;
                                partial.publish(&mut store, id)?;
                            }
                        }
                    }
                };
                // External cancellation returns only after the proxy closes and
                // the isolated process group is killed and its root reaped.
                drop(operation);
                // Text sent before the run ended still belongs to its reply.
                while let Ok(update) = progress.try_recv() {
                    partial.update(update);
                }
                self.running.lock().map_err(poisoned("run"))?.remove(&id);
                let mut store = self.lock()?;
                let thread = store.thread_mut(id)?;
                match result {
                    Some(Ok((text, usage))) => {
                        partial.finish(thread, text);
                        // A backend reports one total for its whole run.
                        thread.run_usage = Some(usage.clone());
                        thread.usage = Some(usage);
                        thread.state = RunState::Complete;
                    }
                    Some(Err(e)) => {
                        // Partial text stays visible, followed by the failure;
                        // the run is Failed, never Complete.
                        partial.apply(thread);
                        fail_run(thread, id, &e);
                    }
                    None => {
                        partial.apply(thread);
                        thread.state = RunState::Cancelled;
                    }
                }
                let thread = thread.clone();
                store.persist(Some(EventKind::ThreadChanged { id }))?;
                Ok(Reply::Thread(Box::new(thread)))
            }
            Command::ListTools { thread } => self.list_tools(thread).await,
            Command::SetToolPermission {
                thread,
                tool,
                permission,
            } => self.set_tool_permission(thread, tool, permission),
            Command::SetToolBudget { thread, budget } => self.set_tool_budget(thread, budget),
            Command::ResolveApproval {
                thread,
                approval,
                digest,
                decision,
            } => self.resolve_approval(thread, approval, &digest, decision),
            Command::AnswerPrompt {
                thread,
                prompt,
                answer,
            } => {
                self.prompts.answer(thread, prompt, answer)?;
                Ok(Reply::Ok)
            }
            Command::CancelRun { thread } => delegation::cancel_run(self, thread),
            Command::CreateProject { name, root } => {
                check(
                    !name.trim().is_empty(),
                    GatewayError::Invalid("project name is required"),
                )?;
                let root = std::fs::canonicalize(root).map_err(GatewayError::Workspace)?;
                check(
                    root.is_dir(),
                    GatewayError::Invalid("workspace must be a directory"),
                )?;
                let id = Id::new_v4();
                let mut manager = Thread::new(format!("{name} · coordinator"), Mode::Plan);
                manager.project = Some(id);
                let project = Project {
                    id,
                    name,
                    root: root.to_string_lossy().into(),
                    manager: manager.id,
                };
                let mut store = self.lock()?;
                store.state.projects.push(project.clone());
                store.state.threads.push(manager);
                store.persist(Some(EventKind::ProjectChanged { id }))?;
                Ok(Reply::Project(project))
            }
            Command::CreateSubagent {
                parent,
                title,
                mode,
                model,
                requested,
            } => {
                let mut store = self.lock()?;
                validate_model(&store.state, &model)?;
                let parent_thread = store.thread(parent)?.clone();
                check(
                    parent_thread.subagent_models.contains(&model),
                    GatewayError::Denied("model is not permitted for this thread's subagents"),
                )?;
                let mut thread = Thread::new(title, mode);
                thread.project = parent_thread.project;
                thread.parent = Some(parent);
                thread.model = Some(model.clone());
                thread.grant = parent_thread.grant.attenuate(
                    mode,
                    &Grant {
                        capabilities: requested,
                        models: [model].into(),
                    },
                    &mode.ceiling(),
                );
                store.state.threads.push(thread.clone());
                store.persist(Some(EventKind::ThreadChanged { id: thread.id }))?;
                Ok(Reply::Thread(Box::new(thread)))
            }
            Command::Download {
                url,
                name,
                sha256,
                models_directory,
            } => {
                self.start_download(url, name, sha256, models_directory)
                    .await
            }
            Command::CancelDownload { id } => {
                let mut store = self.lock()?;
                let download = store
                    .state
                    .downloads
                    .iter_mut()
                    .find(|d| d.id == id)
                    .ok_or(GatewayError::NotFound("download not found"))?;
                check(
                    matches!(
                        download.state,
                        DownloadState::Running | DownloadState::Queued
                    ),
                    GatewayError::Conflict("download is no longer running"),
                )?;
                download.state = DownloadState::Cancelled;
                if let Some(token) = self
                    .downloads
                    .lock()
                    .map_err(poisoned("transfer"))?
                    .get(&id)
                {
                    token.cancel();
                }
                let event = download.clone();
                store.persist(Some(EventKind::DownloadChanged(event)))?;
                Ok(Reply::Ok)
            }
            Command::GitStatus { project } => {
                let root = self.project_root(project)?;
                let output = crate::process::output(
                    tokio::process::Command::new("git")
                        .args([
                            "--no-pager",
                            "--no-optional-locks",
                            "-c",
                            "core.fsmonitor=false",
                            "status",
                            "--short",
                            "--branch",
                        ])
                        .env_remove("GIT_DIR")
                        .env_remove("GIT_WORK_TREE")
                        .env_remove("GIT_INDEX_FILE")
                        .current_dir(root),
                    512 * 1024,
                    std::time::Duration::from_secs(15),
                )
                .await?;
                Ok(Reply::Text(String::from_utf8_lossy(&output).into_owned()))
            }
            Command::ReadDocument { project, path } => {
                let root = self.project_root(project)?;
                let text = tokio::task::spawn_blocking(move || -> Result<String> {
                    use std::io::Read;
                    let directory =
                        cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority())
                            .map_err(GatewayError::Workspace)?;
                    let file = directory.open(path).map_err(GatewayError::Document)?;
                    let mut text = String::new();
                    file.take(512 * 1024 + 1)
                        .read_to_string(&mut text)
                        .map_err(GatewayError::NotText)?;
                    check(
                        text.len() <= 512 * 1024,
                        GatewayError::Invalid("text preview is limited to 512 KiB"),
                    )?;
                    Ok(text)
                })
                .await??;
                Ok(Reply::Text(text))
            }
            Command::OpenPanel {
                extension,
                panel,
                project,
                event,
            } => {
                let scope = self.user_scope(project)?;
                let view = self
                    .extensions
                    .open_panel(&extension, &panel, event, scope)
                    .await?;
                Ok(Reply::Panel(view))
            }
            Command::CallTool {
                extension,
                tool,
                args,
                project,
            } => {
                let scope = self.user_scope(project)?;
                let output = self
                    .extensions
                    .call_tool(&extension, &tool, args, scope)
                    .await?;
                Ok(Reply::Text(output.text))
            }
            Command::GrantExtension {
                extension,
                capabilities,
            } => {
                let mut store = self.lock()?;
                let plugin = store
                    .state
                    .plugins
                    .iter_mut()
                    .find(|p| p.id == extension)
                    .ok_or(GatewayError::NotFound("extension not found"))?;
                check(
                    capabilities.is_subset(&plugin.capabilities),
                    GatewayError::Invalid(
                        "an extension can only be granted what its manifest requests",
                    ),
                )?;
                plugin.granted = capabilities;
                store.persist(Some(EventKind::ExtensionChanged { id: extension }))?;
                drop(store);
                // Running extension calls and approval waits re-check that
                // their tool is still offered.
                self.policy_changed();
                Ok(Reply::Ok)
            }
            Command::EnableExtension { extension, enabled } => {
                let mut store = self.lock()?;
                store
                    .state
                    .plugins
                    .iter_mut()
                    .find(|p| p.id == extension)
                    .ok_or(GatewayError::NotFound("extension not found"))?
                    .enabled = enabled;
                store.persist(Some(EventKind::ExtensionChanged { id: extension }))?;
                drop(store);
                // Running extension calls and approval waits re-check that
                // their tool is still offered.
                self.policy_changed();
                Ok(Reply::Ok)
            }
            Command::InstallExtension { manifest, module } => {
                let summary = self.extensions.install(manifest, module).await?;
                self.policy_changed();
                Ok(Reply::Extension(Box::new(summary)))
            }
            Command::UninstallExtension { extension } => {
                self.extensions.uninstall(extension).await?;
                // Running calls and approval waits re-check their tool.
                self.policy_changed();
                Ok(Reply::Ok)
            }
            Command::InstallProposal { digest } => {
                let summary = self.extensions.install_proposal(digest).await?;
                self.policy_changed();
                Ok(Reply::Extension(Box::new(summary)))
            }
            Command::DismissProposal { digest } => {
                self.extensions.dismiss_proposal(digest).await?;
                Ok(Reply::Ok)
            }
            Command::Skills => Ok(Reply::Skills(self.skills()?)),
            Command::ReadSkill { name } => Ok(Reply::Skill(self.read_skill(&name)?)),
            Command::SetSkillEnabled {
                name,
                enabled,
                digest,
            } => {
                self.set_skill_enabled(&name, enabled, digest.as_deref())?;
                Ok(Reply::Ok)
            }
            Command::ConnectMcpServer { name } => {
                self.connect_mcp(&name).await?;
                Ok(Reply::Ok)
            }
            Command::DisconnectMcpServer { name } => {
                self.disconnect_mcp(&name)?;
                Ok(Reply::Ok)
            }
            Command::Shutdown => {
                // Serialize independent shutdown attempts so a failed one's
                // rollback cannot reopen admission during another one's cleanup.
                let mut committed = self.shutdown_committed.lock().await;
                if *committed {
                    return Ok(Reply::Ok);
                }
                let mut drain = DrainAttempt::begin(&self.admission)?;
                tokio::time::timeout(std::time::Duration::from_secs(15), async {
                    loop {
                        // Admission is closed. Repeat cancellation for requests
                        // admitted before stop that only now acquired their task token.
                        for token in self.running.lock().map_err(poisoned("run"))?.values() {
                            token.cancel();
                        }
                        for token in self
                            .downloads
                            .lock()
                            .map_err(poisoned("download"))?
                            .values()
                        {
                            token.cancel();
                        }
                        let runs = self.running.lock().map_err(poisoned("run"))?.len();
                        let downloads = self.downloads.lock().map_err(poisoned("download"))?.len();
                        let active = self
                            .admission
                            .lock()
                            .map_err(poisoned("gateway admission"))?
                            .active;
                        if active == 0 && runs == 0 && downloads == 0 {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    }
                    self.joshua.lock().await.stop().await?;
                    self.clear_managed_joshua()?;
                    self.mcp.stop_all()?;
                    Ok::<_, GatewayError>(())
                })
                .await
                .map_err(GatewayError::ShutdownTimeout)??;
                // Persist user intent before acknowledging stop. Ordinary
                // clients cannot resurrect a deliberately stopped service.
                crate::service::mark_stopped(&self.lock()?.root)
                    .map_err(GatewayError::MarkStopped)?;
                drain.committed = true;
                *committed = true;
                Ok(Reply::Ok)
            }
        }
    }
    /// The scope of a user acting directly: their grant to the extension,
    /// on the project they chose.
    fn user_scope(&self, project: Option<Id>) -> Result<crate::extension_host::Scope> {
        Ok(crate::extension_host::Scope {
            workspace: project.map(|id| self.project_root(id)).transpose()?,
            ceiling: None,
        })
    }
    /// The extension host the run engine calls tools through.
    pub fn extensions(&self) -> &crate::extension_host::ExtensionHost {
        &self.extensions
    }
    fn project_root(&self, id: Id) -> Result<PathBuf> {
        Ok(self
            .lock()?
            .state
            .projects
            .iter()
            .find(|p| p.id == id)
            .ok_or(GatewayError::NotFound("project not found"))?
            .root
            .clone()
            .into())
    }
    async fn start_download(
        &self,
        url: String,
        name: String,
        sha256: Option<String>,
        models_directory: bool,
    ) -> Result<Reply> {
        let parsed = url::Url::parse(&url).map_err(GatewayError::DownloadUrl)?;
        check(
            parsed.scheme() == "https"
                && parsed.username().is_empty()
                && parsed.password().is_none(),
            GatewayError::Invalid("downloads require credential-free HTTPS URLs"),
        )?;
        check(
            !name.is_empty()
                && name.len() < 200
                && !name.starts_with('.')
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
            GatewayError::Invalid("use a simple filename without directories"),
        )?;
        if let Some(hash) = &sha256 {
            check(
                hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
                GatewayError::Invalid("SHA-256 must contain 64 hexadecimal characters"),
            )?;
        }
        let download = Download {
            id: Id::new_v4(),
            name,
            source: parsed.origin().ascii_serialization(),
            received: 0,
            total: None,
            state: DownloadState::Queued,
            error: None,
            sha256,
            failure: None,
        };
        let target = {
            let mut store = self.lock()?;
            let target = if models_directory {
                crate::joshua::models_directory()?
            } else {
                store.root.join("downloads")
            };
            std::fs::create_dir_all(&target).map_err(GatewayError::DownloadDirectory)?;
            check(
                !target.join(&download.name).exists(),
                GatewayError::Conflict("destination already exists"),
            )?;
            check(
                !store.state.downloads.iter().any(|d| {
                    d.name == download.name
                        && matches!(d.state, DownloadState::Running | DownloadState::Queued)
                }),
                GatewayError::Conflict("filename already has an active transfer"),
            )?;
            store.state.downloads.push(download.clone());
            store.persist(Some(EventKind::DownloadChanged(download.clone())))?;
            target
        };
        let token = tokio_util::sync::CancellationToken::new();
        self.downloads
            .lock()
            .map_err(poisoned("transfer"))?
            .insert(download.id, token.clone());
        let gateway = self.clone();
        let job = download.clone();
        tokio::spawn(async move {
            let result = tokio::select! { result = gateway.transfer(&job, &target, &url) => result, _ = token.cancelled() => Err(GatewayError::DownloadCancelled) };
            if let Err(error) = result {
                let report = crate::error::report(&error).with_correlation(job.id);
                let update = gateway.update_download(job.id, |d| {
                    if d.state != DownloadState::Cancelled {
                        d.state = DownloadState::Failed;
                        crate::error::record("download", &report);
                        d.fail(report);
                    }
                });
                if let Err(error) = update {
                    crate::error::handle("persist_download_failure", &error);
                }
            }
            if let Ok(mut tokens) = gateway.downloads.lock() {
                tokens.remove(&job.id);
            }
        });
        Ok(Reply::Download(download))
    }
    fn update_download(&self, id: Id, update: impl FnOnce(&mut Download)) -> Result<Download> {
        let mut store = self.lock()?;
        let download = store
            .state
            .downloads
            .iter_mut()
            .find(|d| d.id == id)
            .ok_or(GatewayError::NotFound("download disappeared"))?;
        update(download);
        let copy = download.clone();
        store.persist(Some(EventKind::DownloadChanged(copy.clone())))?;
        Ok(copy)
    }
    async fn transfer(&self, download: &Download, target: &Path, source: &str) -> Result<()> {
        let response = crate::network::public_get(source).await?;
        self.transfer_response(download, target, response).await
    }
    async fn transfer_response(
        &self,
        download: &Download,
        target: &Path,
        response: reqwest::Response,
    ) -> Result<()> {
        let total = response.content_length();
        let updated = self.update_download(download.id, |d| {
            if d.state != DownloadState::Cancelled {
                d.state = DownloadState::Running;
                d.total = total;
            }
        })?;
        check(
            updated.state != DownloadState::Cancelled,
            GatewayError::DownloadCancelled,
        )?;
        // Create synchronously under an owning guard: cancellation must not race an
        // unfinished asynchronous open, leaving a file with no cleanup owner.
        let partial = tempfile::Builder::new()
            .prefix(&format!("{}.{}.", download.name, download.id))
            .suffix(".part")
            .tempfile_in(target)
            .map_err(GatewayError::Partial)?;
        let mut file = tokio::fs::File::from_std(partial.reopen().map_err(GatewayError::Partial)?);
        let mut stream = response.bytes_stream();
        let mut hasher = Sha256::new();
        let mut received = 0;
        let mut last_event = std::time::Instant::now();
        while let Some(bytes) = stream.next().await {
            let bytes = bytes.map_err(crate::network::transport)?;
            let cancelled = self
                .lock()?
                .state
                .downloads
                .iter()
                .any(|d| d.id == download.id && d.state == DownloadState::Cancelled);
            check(!cancelled, GatewayError::DownloadCancelled)?;
            file.write_all(&bytes)
                .await
                .map_err(GatewayError::Partial)?;
            hasher.update(&bytes);
            received += bytes.len() as u64;
            if last_event.elapsed() >= std::time::Duration::from_millis(250) {
                self.update_download(download.id, |d| d.received = received)?;
                last_event = std::time::Instant::now();
            }
        }
        file.sync_all().await.map_err(GatewayError::Partial)?;
        drop(file);
        let digest = format!("{:x}", hasher.finalize());
        check(
            download
                .sha256
                .as_ref()
                .is_none_or(|expected| expected.eq_ignore_ascii_case(&digest)),
            GatewayError::Integrity,
        )?;
        if let Some(expected) = total {
            check(
                expected == received,
                GatewayError::Length { expected, received },
            )?;
        }
        // Publication is serialized with cancellation. hard_link refuses to replace an existing target.
        let mut store = self.lock()?;
        let current = store
            .state
            .downloads
            .iter_mut()
            .find(|d| d.id == download.id)
            .ok_or(GatewayError::NotFound("download disappeared"))?;
        check(
            current.state != DownloadState::Cancelled,
            GatewayError::DownloadCancelled,
        )?;
        partial
            .persist_noclobber(target.join(&download.name))
            .map_err(|error| GatewayError::Publish(error.error))?;
        current.received = received;
        current.sha256 = Some(digest);
        current.state = DownloadState::Complete;
        let event = current.clone();
        store.persist(Some(EventKind::DownloadChanged(event)))?;
        Ok(())
    }
}
pub fn scoped_path(root: &Path, path: &str) -> Result<PathBuf> {
    check(
        !Path::new(path).is_absolute(),
        GatewayError::Invalid("document path must be relative to the project"),
    )?;
    let root = root.canonicalize().map_err(GatewayError::Workspace)?;
    let target = root
        .join(path)
        .canonicalize()
        .map_err(GatewayError::Document)?;
    check(
        target.starts_with(&root),
        GatewayError::Denied("document escapes the project"),
    )?;
    Ok(target)
}
/// Replace `settings.toml` atomically with `settings`.
fn write_settings(root: &Path, settings: &Settings) -> Result<()> {
    let text = toml::to_string_pretty(settings).map_err(GatewayError::EncodeSettings)?;
    let temporary = root.join("settings.toml.new");
    std::fs::write(&temporary, text)
        .and_then(|()| std::fs::File::open(&temporary)?.sync_all())
        .and_then(|()| std::fs::rename(&temporary, root.join("settings.toml")))
        .map_err(GatewayError::WriteSettings)
}
/// Mark a run Failed with its structured report, shown after any partial text.
fn fail_run(thread: &mut Thread, id: Id, error: &(dyn std::error::Error + 'static)) {
    let report = crate::error::correlated("run", id, error);
    thread.state = RunState::Failed;
    thread.messages.push(Message::new(
        Role::System,
        report.detail(),
        now(),
        thread.backend,
    ));
    thread.error = Some(report);
}
fn validate_model(snapshot: &Snapshot, key: &ModelKey) -> Result<()> {
    check(
        snapshot
            .catalogs
            .iter()
            .any(|c| !c.stale(now()) && c.models.iter().any(|m| &m.key == key)),
        GatewayError::Conflict(
            "model is unavailable or its API catalog has expired; refresh the provider",
        ),
    )?;
    Ok(())
}
pub async fn serve_connection<R, W>(gateway: Gateway, mut input: R, mut output: W) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (sender, mut receiver) =
        tokio::sync::mpsc::channel::<(Response<Reply>, StopAfterReply)>(32);
    let writer = tokio::spawn(async move {
        while let Some((reply, stop)) = receiver.recv().await {
            let delivered = transport::write_frame(&mut output, &reply).await;
            // Cleanup and current stop intent are already committed. A lost
            // acknowledgement must not leave a live gateway stuck draining.
            drop(stop);
            delivered?;
        }
        Ok::<(), transport::FrameError>(())
    });
    let semaphore = Arc::new(tokio::sync::Semaphore::new(16));
    let mut tasks = tokio::task::JoinSet::new();
    let stop = gateway.stop_signal();
    loop {
        let request = tokio::select! {
            _ = stop.cancelled() => break,
            request = transport::read_frame::<Request<Command>>(&mut input) => request?,
        };
        let Some(request) = request else { break };
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(GatewayError::Limiter)?;
        let gateway = gateway.clone();
        let sender = sender.clone();
        tasks.spawn(async move {
            let _permit = permit;
            let (reply, stop) = gateway.request_with_effect(request).await;
            let stop = StopAfterReply(stop.then(|| gateway.stop_signal()));
            sender
                .send((reply, stop))
                .await
                .map_err(|_| GatewayError::ReplyChannel)
        });
        while let Some(result) = tasks.try_join_next() {
            result??;
        }
    }
    while let Some(result) = tasks.join_next().await {
        result??;
    }
    drop(sender);
    writer.await??;
    Ok(())
}

fn validate_effort(
    snapshot: &Snapshot,
    model: Option<&ModelKey>,
    effort: Option<&str>,
) -> Result<()> {
    if let Some(effort) = effort {
        let supported = model.and_then(|key| {
            snapshot
                .catalogs
                .iter()
                .flat_map(|c| &c.models)
                .find(|m| &m.key == key)
        });
        check(
            supported.is_some_and(|m| m.reasoning_efforts.iter().any(|v| v == effort)),
            GatewayError::Invalid("effort is not advertised by the selected model API"),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn committed_shutdown_survives_a_lost_acknowledgement() {
        let directory = tempfile::tempdir().unwrap();
        let gateway = Gateway::open(directory.path()).unwrap();
        let (mut client, server) = tokio::io::duplex(1024);
        let (read, write) = tokio::io::split(server);
        let worker = tokio::spawn(serve_connection(gateway.clone(), read, write));
        transport::write_frame(
            &mut client,
            &Request {
                id: Id::new_v4(),
                body: Command::Shutdown,
            },
        )
        .await
        .unwrap();
        drop(client);
        tokio::time::timeout(std::time::Duration::from_secs(3), gateway.stop.cancelled())
            .await
            .unwrap();
        assert!(crate::service::is_stopped(directory.path()));
        assert!(worker.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn managed_joshua_connection_cannot_be_repointed_or_removed() {
        let directory = tempfile::tempdir().unwrap();
        let gateway = Gateway::open(directory.path()).unwrap();
        let provider = ProviderPreset::builtins()[0].configure("http://127.0.0.1:9876/v1".into());
        {
            let mut store = gateway.lock().unwrap();
            store.state.settings.providers.push(provider.clone());
            store.state.managed_joshua_provider = Some(provider.id);
            store.persist(None).unwrap();
        }
        let mut rename = gateway.snapshot().unwrap().settings;
        rename.providers[0].label = Id::new_v4().to_string();
        assert_eq!(
            gateway
                .request(Request {
                    id: Id::new_v4(),
                    body: Command::SaveSettings(rename)
                })
                .await
                .body,
            Reply::Ok
        );
        let before = gateway.snapshot().unwrap();
        for edit in ["endpoint", "protocol", "credentials", "remove"] {
            let mut settings = before.settings.clone();
            match edit {
                "endpoint" => settings.providers[0].base_url = "http://127.0.0.1:9877/v1".into(),
                "protocol" => settings.providers[0].api = ProviderApi::Anthropic,
                "credentials" => settings.providers[0].credential = Some(Id::new_v4().to_string()),
                "remove" => settings.providers.clear(),
                _ => unreachable!(),
            }
            let request = Id::new_v4();
            let reply = gateway
                .request(Request {
                    id: request,
                    body: Command::SaveSettings(settings),
                })
                .await
                .body;
            // The rejection reaches the client intact and names its request.
            assert!(
                matches!(reply, Reply::Error(ref report)
                    if report.message.contains("stop managed Joshua")
                        && report.correlation == Some(request)),
                "{reply:?}"
            );
            assert_eq!(gateway.snapshot().unwrap(), before);
        }
    }
    #[tokio::test]
    async fn partial_downloads_are_cleaned_on_errors_cancellation_and_publication() {
        use axum::{Router, body::Body, routing::get};
        let payload = Id::new_v4().to_string();
        let sent = payload.clone();
        let waiting = payload.clone();
        let broken = payload.clone();
        let fail = Arc::new(tokio::sync::Notify::new());
        let release = fail.clone();
        let router = Router::new()
            .route(
                "/data",
                get(move || {
                    let data = sent.clone();
                    async move { data }
                }),
            )
            .route(
                "/waiting",
                get(move || {
                    let data = waiting.clone();
                    async move {
                        Body::from_stream(
                            futures_util::stream::once(
                                async move { Ok::<_, std::io::Error>(data) },
                            )
                            .chain(futures_util::stream::pending()),
                        )
                    }
                }),
            )
            .route(
                "/broken",
                get(move || {
                    let data = broken.clone();
                    let release = release.clone();
                    async move {
                        Body::from_stream(
                            futures_util::stream::once(
                                async move { Ok::<_, std::io::Error>(data) },
                            )
                            .chain(futures_util::stream::once(
                                async move {
                                    release.notified().await;
                                    Err::<String, _>(std::io::Error::other("interrupted transfer"))
                                },
                            )),
                        )
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let root = tempfile::tempdir().unwrap();
        let gateway = Gateway::open(root.path()).unwrap();
        for scenario in ["integrity", "network", "cancel", "collision", "complete"] {
            let directory = tempfile::tempdir().unwrap();
            let job = Download {
                id: Id::new_v4(),
                name: Id::new_v4().to_string(),
                source: address.to_string(),
                received: 0,
                total: None,
                state: DownloadState::Queued,
                error: None,
                sha256: (scenario == "integrity").then(|| "0".repeat(64)),
                failure: None,
            };
            gateway.lock().unwrap().state.downloads.push(job.clone());
            if scenario == "collision" {
                std::fs::write(directory.path().join(&job.name), "existing").unwrap();
            }
            let path = match scenario {
                "network" => "broken",
                "cancel" => "waiting",
                _ => "data",
            };
            let response = reqwest::get(format!("http://{address}/{path}"))
                .await
                .unwrap();
            let worker = gateway.clone();
            let download = job.clone();
            let target = directory.path().to_owned();
            let task = tokio::spawn(async move {
                worker.transfer_response(&download, &target, response).await
            });
            if matches!(scenario, "network" | "cancel") {
                tokio::time::timeout(std::time::Duration::from_secs(3), async {
                    loop {
                        if std::fs::read_dir(directory.path())
                            .unwrap()
                            .any(|entry| entry.unwrap().metadata().unwrap().len() > 0)
                        {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                })
                .await
                .unwrap();
            }
            if scenario == "cancel" {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            } else {
                if scenario == "network" {
                    fail.notify_one();
                }
                let result = tokio::time::timeout(std::time::Duration::from_secs(3), task)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    result.is_ok(),
                    scenario == "complete",
                    "{scenario}: {result:?}"
                );
            }
            let files: Vec<_> = std::fs::read_dir(directory.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            if matches!(scenario, "collision" | "complete") {
                assert_eq!(files, vec![std::ffi::OsString::from(&job.name)]);
                assert_eq!(
                    std::fs::read_to_string(directory.path().join(&job.name)).unwrap(),
                    if scenario == "collision" {
                        "existing"
                    } else {
                        &payload
                    }
                );
            } else {
                assert!(files.is_empty(), "{scenario}: {files:?}");
            }
        }
        server.abort();
    }
    #[tokio::test]
    async fn command_replay_creates_only_one_thread() {
        let dir = tempfile::tempdir().unwrap();
        let g = Gateway::open(dir.path()).unwrap();
        let id = Id::new_v4();
        let body = Command::CreateThread {
            title: "Work".into(),
            mode: Mode::Code,
        };
        let a = g
            .request(Request {
                id,
                body: body.clone(),
            })
            .await;
        let b = g.request(Request { id, body }).await;
        assert_eq!(a.body, b.body);
        assert_eq!(g.snapshot().unwrap().threads.len(), 1);
    }
    #[tokio::test]
    async fn rejected_commands_reach_the_client_with_their_kind() {
        let dir = tempfile::tempdir().unwrap();
        let g = Gateway::open(dir.path()).unwrap();
        for (body, kind) in [
            (
                Command::CreateThread {
                    title: " ".into(),
                    mode: Mode::Code,
                },
                ErrorKind::InvalidRequest,
            ),
            (
                Command::RenameThread {
                    id: Id::new_v4(),
                    title: "Work".into(),
                },
                ErrorKind::NotFound,
            ),
            (
                Command::CancelRun {
                    thread: Id::new_v4(),
                },
                ErrorKind::NotFound,
            ),
        ] {
            let id = Id::new_v4();
            let response = g.request(Request { id, body }).await;
            let Reply::Error(report) = response.body else {
                panic!("an invalid command must be rejected");
            };
            assert_eq!(report.kind, kind, "{report:#}");
            assert_eq!(report.correlation, Some(id));
        }
    }
    #[test]
    fn document_scope_rejects_escape() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("project")).unwrap();
        std::fs::write(dir.path().join("outside"), "private").unwrap();
        assert!(scoped_path(&dir.path().join("project"), "../outside").is_err());
    }
}
