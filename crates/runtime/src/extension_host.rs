//! The gateway side of extension execution: policy, the worker process and
//! the host calls it relays.
//!
//! Every invocation runs in a fresh `agent206 extension-worker` child with a
//! cleared environment; on Linux that child is also confined by
//! `a206_sandbox`, with no network, no project files and nothing to execute
//! but itself. The worker holds no authority; each host call it
//! relays is checked here against the call's effective grant, which is the
//! intersection of the user's grant to the extension, the caller's scope and
//! what the tool or panel declares.
use crate::store::{StatePut, StateQuota, Store, StoreError};
use a206_core::{
    Capability, ErrorKind, ErrorReport, PluginSource, PluginSummary,
    extension::{Args, ToolOutput, ToolSpec, UiEvent, View},
};
use a206_extensions::{
    ArgError, Limits, Manifest, ManifestError,
    abi::{
        ABI, Call, HostErrorCode, HostFailure, HostReply, HostRequest, Invocation, LogLevel,
        Outcome,
    },
    worker::{Frame, Start},
};
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Wall-clock bound on one invocation, host calls included.
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// Wall-clock bound on one git host call, metadata checks included.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(5);
/// Time a git write keeps in reserve before the invocation's deadline, for
/// the commit and index write after its point of no return and the reply.
pub const GIT_WRITE_RESERVE: Duration = Duration::from_secs(3);
/// Largest file a guest may read in one call.
pub const MAX_FILE: u64 = 512 * 1024;
/// Most entries one directory listing returns.
pub const MAX_ENTRIES: usize = 1024;
/// Persisted state per extension.
pub const STATE_QUOTA: StateQuota = StateQuota {
    entries: 64,
    bytes: 256 * 1024,
};
const MAX_KEY: usize = 128;
const MAX_LOGS: u32 = 32;
const MAX_LOG_MESSAGE: usize = 1024;

/// Why an extension call failed.
#[derive(Debug, thiserror::Error)]
pub enum ExtensionError {
    #[error("extension `{0}` is not installed")]
    NotInstalled(String),
    #[error("extension `{0}` is disabled")]
    Disabled(String),
    #[error("the bundled extensions are invalid")]
    Builtins(#[source] ManifestError),
    #[error(transparent)]
    Package(#[from] packages::PackageError),
    #[error("\"{0}\" is bundled with Agent 206; disable it instead")]
    Bundled(String),
    #[error("\"{0}\" was updated as it started; try again")]
    Updated(String),
    #[error("no prepared extension package has that digest")]
    NotProposed,
    #[error(
        "{} prepared extension packages are already waiting for review; ask the user to install or dismiss one first",
        a206_core::MAX_EXTENSION_PROPOSALS
    )]
    TooManyProposals,
    #[error("\"{0}\" is already installed with exactly this package")]
    AlreadyInstalled(String),
    #[error("could not record the extension")]
    Store(#[from] StoreError),
    #[error("the extension install task failed")]
    Join(#[source] tokio::task::JoinError),
    #[error(transparent)]
    Call(#[from] ArgError),
    #[error("\"{extension}\" needs the \"{}\" permission", capability.label())]
    NotGranted {
        extension: String,
        capability: Capability,
    },
    #[error("this conversation does not allow \"{}\"", capability.label())]
    OutOfScope { capability: Capability },
    #[error("choose a project: this extension works with project files")]
    NoProject,
    #[error("could not read gateway state")]
    Poisoned,
    #[error("could not locate the agent206 executable")]
    CurrentExe(#[source] std::io::Error),
    #[error("could not start the extension worker")]
    Spawn(#[source] std::io::Error),
    #[error("could not confine the extension worker")]
    Confine(#[from] crate::sandbox::ConfineError),
    #[error("the extension worker channel failed")]
    Channel(#[source] std::io::Error),
    #[error("the extension worker sent an invalid frame")]
    Frame(#[source] a206_protocol::Error),
    #[error("the extension worker exited without a result")]
    Exited,
    #[error("extension did not finish within {} seconds", TIMEOUT.as_secs())]
    Timeout(#[source] tokio::time::error::Elapsed),
    /// The worker's engine failed; the report keeps its cause chain.
    #[error("extension failed")]
    Failed(#[source] ErrorReport),
    /// The guest reported a failure. `cause` is the last host call the
    /// gateway itself refused or failed, never the guest's claim about it.
    #[error("{message}")]
    Guest {
        code: String,
        message: String,
        #[source]
        cause: Option<HostFailure>,
    },
    #[error("extension answered with the wrong kind of result")]
    Unexpected,
}
impl crate::error::Classified for ExtensionError {
    fn kind(&self) -> Option<ErrorKind> {
        use crate::error::classify;
        Some(match self {
            Self::NotInstalled(_) => ErrorKind::NotFound,
            Self::Disabled(_) | Self::NotGranted { .. } | Self::OutOfScope { .. } => {
                ErrorKind::PermissionDenied
            }
            Self::NoProject | Self::Bundled(_) => ErrorKind::InvalidRequest,
            Self::Package(error) => return classify(error),
            Self::Store(error) => return classify(error),
            Self::Join(_) => ErrorKind::Internal,
            Self::Updated(_) | Self::TooManyProposals | Self::AlreadyInstalled(_) => {
                ErrorKind::Conflict
            }
            Self::NotProposed => ErrorKind::NotFound,
            Self::Builtins(_) | Self::Poisoned | Self::CurrentExe(_) => ErrorKind::Internal,
            Self::Confine(error) => return classify(error),
            Self::Spawn(_) | Self::Channel(_) | Self::Exited => ErrorKind::Unavailable,
            Self::Frame(_) | Self::Unexpected => ErrorKind::Protocol,
            Self::Timeout(_) => ErrorKind::Timeout,
            Self::Call(error) => return classify(error),
            Self::Failed(_) | Self::Guest { .. } => return None,
        })
    }
}
impl crate::error::Classified for HostFailure {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self.code {
            HostErrorCode::Denied => ErrorKind::PermissionDenied,
            HostErrorCode::NoWorkspace | HostErrorCode::Invalid | HostErrorCode::TooLarge => {
                ErrorKind::InvalidRequest
            }
            HostErrorCode::NotFound => ErrorKind::NotFound,
            HostErrorCode::Budget | HostErrorCode::Exists | HostErrorCode::Conflict => {
                ErrorKind::Conflict
            }
            HostErrorCode::Unsupported | HostErrorCode::Other => return None,
            HostErrorCode::Io => ErrorKind::Storage,
            HostErrorCode::Timeout => ErrorKind::Timeout,
            HostErrorCode::Network => ErrorKind::Unavailable,
        })
    }
}

/// Who is calling and what they may use.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    /// The project root for workspace host calls.
    pub workspace: Option<PathBuf>,
    /// The caller's own ceiling: a thread's effective grant for agent calls,
    /// or `None` for a user acting directly, bounded only by the user's grant
    /// to the extension.
    pub ceiling: Option<BTreeSet<Capability>>,
}

/// An enabled extension tool, as the run engine offers it to models.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionTool {
    pub extension: String,
    pub spec: ToolSpec,
}

/// What [`ExtensionHost::catalog`] reads.
#[derive(Debug, Clone)]
pub struct Catalog {
    pub plugins: Vec<PluginSummary>,
    pub proposals: Vec<a206_core::ExtensionProposal>,
}
/// Capabilities that only make sense inside a project: a call declaring any
/// of them is refused without one.
pub const PROJECT_CAPABILITIES: [Capability; 4] = [
    Capability::ReadWorkspace,
    Capability::WriteWorkspace,
    Capability::GitRead,
    Capability::GitWrite,
];
/// One enabled extension's tools, or why they could not be read.
#[derive(Debug)]
pub struct ExtensionTools {
    pub extension: String,
    pub tools: Result<Vec<ToolSpec>, ExtensionError>,
}

#[derive(Clone)]
pub struct ExtensionHost {
    store: Arc<Mutex<Store>>,
    net: Arc<dyn crate::network::Net>,
    /// Holds installed packages. Passed to unconfined workers; a confined
    /// worker gets an empty one instead.
    data_dir: PathBuf,
    /// Serializes changes to the package store.
    packages: Arc<Mutex<()>>,
}
/// What an update keeps of the grants the user gave the version it replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grants {
    /// The user supplied the new package: keep what its manifest still requests.
    Keep,
    /// An agent prepared it: nothing, so the user reviews every permission again.
    Clear,
}
/// An extension's validated manifest, with its module when it is installed
/// rather than bundled.
struct Loaded {
    manifest: Manifest,
    module: Option<Vec<u8>>,
    /// The recorded source this was loaded from.
    source: PluginSource,
}
impl ExtensionHost {
    pub fn new(store: Arc<Mutex<Store>>, data_dir: PathBuf) -> Self {
        let host = Self {
            store,
            net: Arc::new(crate::network::Internet),
            data_dir,
            packages: Arc::default(),
        };
        if let Err(error) = host.prune() {
            // Leftover files only take space; the next start tries again.
            tracing::warn!(
                error = &error as &dyn std::error::Error,
                "could not prune extension packages"
            );
        }
        host
    }
    /// Remove package files that no installed extension or proposal records.
    fn prune(&self) -> Result<(), ExtensionError> {
        let _serial = self.packages.lock().map_err(|_| ExtensionError::Poisoned)?;
        let keep = {
            let store = self.store.lock().map_err(|_| ExtensionError::Poisoned)?;
            let installed = store
                .state
                .plugins
                .iter()
                .filter_map(|plugin| match &plugin.source {
                    PluginSource::Installed { digest } => Some(digest.clone()),
                    PluginSource::Bundled => None,
                });
            let proposed = store
                .state
                .extension_proposals
                .iter()
                .map(|proposal| proposal.digest.clone());
            installed.chain(proposed).collect()
        };
        Ok(packages::prune(
            &packages::directory(&self.data_dir),
            &keep,
        )?)
    }

    /// Tools of enabled extensions whose declared capabilities the user has
    /// granted. A caller still checks its own ceiling with [`Self::call_tool`].
    pub fn tools(&self) -> Result<Vec<ExtensionTool>, ExtensionError> {
        Ok(self
            .plugins()?
            .into_iter()
            // SkillToolHost exposes this builtin under its stable legacy names.
            .filter(|plugin| plugin.enabled && plugin.id != crate::skill_tool::EXTENSION)
            .flat_map(|plugin| {
                let granted = plugin.granted;
                let extension = plugin.id;
                plugin
                    .tools
                    .into_iter()
                    .filter(move |tool| tool.capabilities.is_subset(&granted))
                    .map(move |spec| ExtensionTool {
                        extension: extension.clone(),
                        spec,
                    })
            })
            .collect())
    }

    /// Each enabled extension's tools whose declared capabilities the user
    /// has granted, read from its validated manifest (the declaration
    /// [`Self::call_tool`] checks against). An extension whose manifest cannot
    /// be loaded is reported on its own; the others are still listed.
    pub fn tool_sources(&self) -> Result<Vec<ExtensionTools>, ExtensionError> {
        Ok(self
            .plugins()?
            .into_iter()
            .filter(|plugin| plugin.enabled && plugin.id != crate::skill_tool::EXTENSION)
            .map(|plugin| {
                let tools = self.load(&plugin.id).map(|loaded| {
                    loaded
                        .manifest
                        .tools
                        .into_iter()
                        .filter(|tool| tool.capabilities.is_subset(&plugin.granted))
                        .collect()
                });
                ExtensionTools {
                    extension: plugin.id,
                    tools,
                }
            })
            .collect())
    }

    /// Run a tool. Arguments are checked against its declaration first.
    ///
    /// Dropping the future stops the call: the worker child is killed
    /// (`kill_on_drop`) and nothing of the invocation keeps running except a
    /// bounded workspace read already handed to the blocking pool, whose
    /// result is discarded. Extension state written before the drop stays.
    pub async fn call_tool(
        &self,
        extension: &str,
        tool: &str,
        args: Args,
        scope: Scope,
    ) -> Result<ToolOutput, ExtensionError> {
        let loaded = self.load(extension)?;
        let spec = loaded.manifest.tool(tool)?;
        a206_extensions::check_args(spec, &args)?;
        let call = Call::Tool {
            name: tool.into(),
            args,
        };
        let declared = spec.capabilities.clone();
        match self.invoke(loaded, &declared, call, scope).await? {
            Outcome::Tool(output) => Ok(output),
            _ => Err(ExtensionError::Unexpected),
        }
    }

    /// Render a panel, after `event` if the user pressed one of its elements.
    pub async fn open_panel(
        &self,
        extension: &str,
        panel: &str,
        event: Option<UiEvent>,
        scope: Scope,
    ) -> Result<View, ExtensionError> {
        let loaded = self.load(extension)?;
        let spec = loaded.manifest.panel(panel)?;
        let call = Call::Panel {
            id: panel.into(),
            event,
        };
        let declared = spec.capabilities.clone();
        match self.invoke(loaded, &declared, call, scope).await? {
            Outcome::Panel(view) => Ok(view),
            _ => Err(ExtensionError::Unexpected),
        }
    }

    /// Every installed extension and every package waiting for review, as
    /// the user sees them on the Extensions page.
    pub fn catalog(&self) -> Result<Catalog, ExtensionError> {
        let store = self.store.lock().map_err(|_| ExtensionError::Poisoned)?;
        Ok(Catalog {
            plugins: store.state.plugins.clone(),
            proposals: store.state.extension_proposals.clone(),
        })
    }
    fn plugins(&self) -> Result<Vec<PluginSummary>, ExtensionError> {
        Ok(self
            .store
            .lock()
            .map_err(|_| ExtensionError::Poisoned)?
            .state
            .plugins
            .clone())
    }
    /// The manifest and module recorded for `extension`. An installed
    /// package is verified against its digest on every load.
    fn load(&self, extension: &str) -> Result<Loaded, ExtensionError> {
        let not_installed = || ExtensionError::NotInstalled(extension.into());
        let plugin = self
            .plugins()?
            .into_iter()
            .find(|p| p.id == extension)
            .ok_or_else(not_installed)?;
        match plugin.source {
            PluginSource::Bundled => a206_extensions::builtin(extension)
                .map_err(ExtensionError::Builtins)?
                .map(|builtin| Loaded {
                    manifest: builtin.manifest,
                    module: None,
                    source: PluginSource::Bundled,
                })
                .ok_or_else(not_installed),
            PluginSource::Installed { digest } => {
                let package =
                    packages::load(&packages::directory(&self.data_dir), extension, &digest)?;
                Ok(Loaded {
                    manifest: package.manifest,
                    module: Some(package.module),
                    source: PluginSource::Installed { digest },
                })
            }
        }
    }

    /// Install or update a third-party package (user operation). It arrives
    /// disabled; an update keeps only the grants its new manifest requests.
    /// Installing the package already installed changes nothing, apart from
    /// restoring its stored copy if that was altered.
    pub async fn install(
        &self,
        manifest: String,
        module: Vec<u8>,
    ) -> Result<PluginSummary, ExtensionError> {
        let package = packages::prepare(manifest, module)?;
        let host = self.clone();
        tokio::task::spawn_blocking(move || host.install_blocking(package))
            .await
            .map_err(ExtensionError::Join)?
    }
    fn install_blocking(
        &self,
        package: packages::Package,
    ) -> Result<PluginSummary, ExtensionError> {
        let _serial = self.packages.lock().map_err(|_| ExtensionError::Poisoned)?;
        self.install_locked(package, Grants::Keep)
    }
    /// Store and record `package`, withdrawing any proposal of the same
    /// package. The caller holds the package lock.
    fn install_locked(
        &self,
        package: packages::Package,
        grants: Grants,
    ) -> Result<PluginSummary, ExtensionError> {
        let dir = packages::directory(&self.data_dir);
        let created = packages::store(&dir, &package)?;
        // A package directory this install created is removed again if the
        // install is not recorded; nothing else refers to it.
        let discard = |error: ExtensionError| {
            if created && let Err(cleanup) = packages::remove(&dir, &package.digest) {
                tracing::warn!(error = &cleanup as &dyn std::error::Error, extension = %package.manifest.id, "could not remove an unrecorded extension package");
            }
            error
        };
        let id = package.manifest.id.clone();
        let mut store = self
            .store
            .lock()
            .map_err(|_| discard(ExtensionError::Poisoned))?;
        let plugins = &mut store.state.plugins;
        let previous = plugins.iter().position(|p| p.id == id);
        let installed = PluginSource::Installed {
            digest: package.digest.clone(),
        };
        let granted = match previous.map(|index| &plugins[index]) {
            // Unreachable while bundled ids keep the reserved prefix.
            Some(plugin) if plugin.source == PluginSource::Bundled => {
                return Err(discard(ExtensionError::Bundled(id)));
            }
            // The same package again: nothing the user reviewed changed.
            Some(plugin) if plugin.source == installed => return Ok(plugin.clone()),
            Some(plugin) if grants == Grants::Keep => plugin.granted.clone(),
            Some(_) | None => BTreeSet::new(),
        };
        let mut summary = package.manifest.summary(false, &granted);
        summary.source = installed;
        let replaced = match previous {
            Some(index) => Some(std::mem::replace(&mut plugins[index], summary.clone())),
            None => {
                plugins.push(summary.clone());
                None
            }
        };
        // The files now belong to the installed extension.
        store
            .state
            .extension_proposals
            .retain(|proposal| proposal.digest != package.digest);
        // A refused persist restores the earlier state.
        store
            .persist(Some(a206_core::EventKind::ExtensionChanged {
                id: id.clone(),
            }))
            .map_err(|error| discard(error.into()))?;
        drop(store);
        if let Some(PluginSource::Installed { digest }) = replaced.map(|p| p.source)
            && let Err(error) = packages::remove(&dir, &digest)
        {
            // The new package is recorded; the leftover copy is pruned at the
            // next start.
            tracing::warn!(error = &error as &dyn std::error::Error, extension = %id, "could not remove the replaced extension package");
        }
        Ok(summary)
    }

    /// Remove an installed extension, its stored state and its package.
    pub async fn uninstall(&self, extension: String) -> Result<(), ExtensionError> {
        let host = self.clone();
        tokio::task::spawn_blocking(move || host.uninstall_blocking(&extension))
            .await
            .map_err(ExtensionError::Join)?
    }
    fn uninstall_blocking(&self, extension: &str) -> Result<(), ExtensionError> {
        let _serial = self.packages.lock().map_err(|_| ExtensionError::Poisoned)?;
        let mut store = self.store.lock().map_err(|_| ExtensionError::Poisoned)?;
        let index = store
            .state
            .plugins
            .iter()
            .position(|p| p.id == extension)
            .ok_or_else(|| ExtensionError::NotInstalled(extension.into()))?;
        let PluginSource::Installed { digest } = store.state.plugins[index].source.clone() else {
            return Err(ExtensionError::Bundled(extension.into()));
        };
        // State first: if recording the removal then fails, the extension is
        // still listed and uninstalling again finishes the job.
        store.clear_extension_state(extension)?;
        store.state.plugins.remove(index);
        store.persist(Some(a206_core::EventKind::ExtensionChanged {
            id: extension.into(),
        }))?;
        drop(store);
        // On failure the files are no longer recorded and are pruned at the
        // next start; the error says so.
        packages::remove(&packages::directory(&self.data_dir), &digest)?;
        Ok(())
    }

    async fn invoke(
        &self,
        loaded: Loaded,
        declared: &BTreeSet<Capability>,
        call: Call,
        scope: Scope,
    ) -> Result<Outcome, ExtensionError> {
        let Loaded {
            manifest,
            module,
            source,
        } = loaded;
        let plugin = self
            .plugins()?
            .into_iter()
            .find(|p| p.id == manifest.id)
            .ok_or_else(|| ExtensionError::NotInstalled(manifest.id.clone()))?;
        if plugin.source != source {
            return Err(ExtensionError::Updated(manifest.name.clone()));
        }
        if !plugin.enabled {
            return Err(ExtensionError::Disabled(manifest.id.clone()));
        }
        for capability in declared {
            if !plugin.granted.contains(capability) {
                return Err(ExtensionError::NotGranted {
                    extension: manifest.name.clone(),
                    capability: *capability,
                });
            }
            if scope
                .ceiling
                .as_ref()
                .is_some_and(|c| !c.contains(capability))
            {
                return Err(ExtensionError::OutOfScope {
                    capability: *capability,
                });
            }
        }
        if scope.workspace.is_none() && PROJECT_CAPABILITIES.iter().any(|c| declared.contains(c)) {
            return Err(ExtensionError::NoProject);
        }
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        let host = HostCalls {
            extension: manifest.id.clone(),
            effective: declared.clone(),
            workspace: scope.workspace,
            skills_root: self.data_dir.join("skills"),
            store: Arc::clone(&self.store),
            hosts: manifest.hosts.clone(),
            net: Arc::clone(&self.net),
            user_git: git_commit::UserGit::load,
            deadline,
            source,
            logs: 0,
            last_failure: None,
        };
        let start = Start {
            extension: manifest.id.clone(),
            module: module.map(serde_bytes::ByteBuf::from),
            invocation: Invocation { abi: ABI, call },
            limits: Limits::default(),
        };
        tokio::time::timeout_at(deadline, self.run_worker(start, host))
            .await
            .map_err(ExtensionError::Timeout)?
    }

    /// Not cancel-safe in the sense of preserving progress: dropping the
    /// future kills the worker (`kill_on_drop`), abandoning the invocation.
    async fn run_worker(
        &self,
        start: Start,
        mut host: HostCalls,
    ) -> Result<Outcome, ExtensionError> {
        let mut worker = self.spawn_worker().await?;
        let child = worker.child();
        let (Some(mut input), Some(mut output)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(ExtensionError::Exited);
        };
        let exchange = async {
            write_frame(&mut input, &start).await?;
            loop {
                match read_frame::<Frame>(&mut output).await? {
                    Frame::Host(request) => {
                        let reply = host.handle(request).await;
                        write_frame(&mut input, &reply).await?;
                    }
                    Frame::Done(result) => break Ok(result),
                }
            }
        };
        let result = match exchange.await {
            Ok(result) => result,
            // A worker that never answered may never have started.
            Err(error @ (ExtensionError::Exited | ExtensionError::Channel(_))) => {
                return Err(match worker.startup_failure().await {
                    Some(start) => crate::sandbox::ConfineError::Start(start).into(),
                    None => error,
                });
            }
            Err(error) => return Err(error),
        };
        let child = worker.child();
        // The worker exits after its result; reap it rather than leave a zombie.
        drop(input);
        if let Err(error) = child.wait().await {
            tracing::warn!(error = &error as &dyn std::error::Error, extension = %start.extension, "could not reap the extension worker");
        }
        worker.stop().await;
        match result.map_err(ExtensionError::Failed)? {
            Outcome::Failed(guest) => Err(ExtensionError::Guest {
                code: guest.code,
                message: guest.message,
                cause: host.last_failure,
            }),
            outcome => Ok(outcome),
        }
    }
}

/// A running worker, confined on Linux and macOS.
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct Worker(crate::sandbox::Confined);
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
struct Worker(tokio::process::Child);
impl Worker {
    fn child(&mut self) -> &mut tokio::process::Child {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        return &mut self.0.child;
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        return &mut self.0;
    }
    async fn stop(self) {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        self.0.stop().await;
    }
    /// Why the sandbox kept the worker from starting, if it did.
    async fn startup_failure(&mut self) -> Option<crate::sandbox::StartError> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        return self.0.startup_failure().await;
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        return None;
    }
}

impl ExtensionHost {
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    async fn spawn_worker(&self) -> Result<Worker, ExtensionError> {
        let child = tokio::process::Command::new(
            std::env::current_exe().map_err(ExtensionError::CurrentExe)?,
        )
        .arg("--data-dir")
        .arg(&self.data_dir)
        .arg("extension-worker")
        .env_clear()
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(ExtensionError::Spawn)?;
        Ok(Worker(child))
    }

    /// A missing sandbox layer is an error, never an unconfined worker.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    async fn spawn_worker(&self) -> Result<Worker, ExtensionError> {
        // The worker keeps nothing; its data directory is the empty scratch.
        let confined = crate::sandbox::Confined::spawn_self(|scratch| {
            vec![
                "--data-dir".into(),
                scratch.into(),
                "extension-worker".into(),
            ]
        })
        .await?;
        Ok(Worker(confined))
    }
}

async fn write_frame<T: serde::Serialize>(
    writer: &mut (impl AsyncWriteExt + Unpin),
    value: &T,
) -> Result<(), ExtensionError> {
    let frame = a206_protocol::encode(value).map_err(ExtensionError::Frame)?;
    writer
        .write_all(&frame)
        .await
        .map_err(ExtensionError::Channel)?;
    writer.flush().await.map_err(ExtensionError::Channel)
}
async fn read_frame<T: serde::de::DeserializeOwned>(
    reader: &mut (impl AsyncReadExt + Unpin),
) -> Result<T, ExtensionError> {
    let eof = |error: std::io::Error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            ExtensionError::Exited
        } else {
            ExtensionError::Channel(error)
        }
    };
    let mut header = [0; a206_protocol::HEADER];
    reader.read_exact(&mut header).await.map_err(eof)?;
    let len = a206_protocol::payload_size(&header).map_err(ExtensionError::Frame)?;
    let mut payload = vec![0; len];
    reader.read_exact(&mut payload).await.map_err(eof)?;
    a206_protocol::decode_payload(&payload).map_err(ExtensionError::Frame)
}

/// Executes relayed host calls for one invocation.
struct HostCalls {
    skills_root: PathBuf,
    extension: String,
    effective: BTreeSet<Capability>,
    workspace: Option<PathBuf>,
    store: Arc<Mutex<Store>>,
    /// The manifest's network destinations.
    hosts: Vec<String>,
    net: Arc<dyn crate::network::Net>,
    /// Reads the user's own git settings for a commit.
    user_git: fn() -> Result<git_commit::UserGit, HostFailure>,
    /// When the whole invocation times out and its pending replies are lost.
    deadline: tokio::time::Instant,
    /// The package this invocation runs; calls stop once it is replaced.
    source: PluginSource,
    logs: u32,
    last_failure: Option<HostFailure>,
}
fn failure(code: HostErrorCode, message: &str) -> HostFailure {
    HostFailure {
        code,
        message: message.into(),
    }
}
impl HostCalls {
    async fn handle(&mut self, request: HostRequest) -> HostReply {
        match self.execute(request).await {
            Ok(reply) => reply,
            Err(failure) => {
                self.last_failure = Some(failure.clone());
                HostReply::Failed(failure)
            }
        }
    }
    async fn skill_reply(
        &self,
        name: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<HostReply, HostFailure> {
        // Only the bundled module itself, never a package installed under any id.
        if self.extension != crate::skill_tool::EXTENSION || self.source != PluginSource::Bundled {
            return Err(failure(
                HostErrorCode::Denied,
                "skill reads are restricted to the skills builtin",
            ));
        }
        let pins = self.lock()?.state.settings.enabled_skills.clone();
        let approved = pins.clone();
        let root = self.skills_root.clone();
        let request = crate::tools::ToolRequest {
            call: String::new(),
            name: name.into(),
            arguments,
            approved: false,
        };
        let text =
            crate::skill_tool::io(move || crate::skill_tool::read(&root, &approved, &request))
                .await
                .map_err(|error| {
                    let report = crate::error::handle("skill_host_read", &error);
                    let code =
                        if matches!(&error, crate::skill_tool::SkillToolError::TooLarge { .. }) {
                            HostErrorCode::TooLarge
                        } else {
                            match report.kind {
                                ErrorKind::PermissionDenied => HostErrorCode::Denied,
                                ErrorKind::NotFound => HostErrorCode::NotFound,
                                ErrorKind::InvalidRequest => HostErrorCode::Invalid,
                                ErrorKind::Timeout => HostErrorCode::Timeout,
                                _ => HostErrorCode::Io,
                            }
                        };
                    failure(code, &report.message)
                })?;
        let store = self.lock()?;
        if store.state.settings.enabled_skills != pins
            || !store
                .state
                .plugins
                .iter()
                .any(|p| p.id == self.extension && p.enabled)
        {
            return Err(failure(
                HostErrorCode::Denied,
                "skill approval or extension permission changed during the read",
            ));
        }
        Ok(HostReply::File(text.into_bytes().into()))
    }
    async fn execute(&mut self, request: HostRequest) -> Result<HostReply, HostFailure> {
        // The stored plugin is read again for every call, so disabling or
        // revoking takes effect inside an invocation already running.
        let required = a206_extensions::required_capability(&request);
        let allowed = self.lock()?.state.plugins.iter().any(|p| {
            p.id == self.extension
                && p.source == self.source
                && p.enabled
                && required.is_none_or(|c| p.granted.contains(&c) && self.effective.contains(&c))
        });
        if !allowed {
            let message = match required {
                Some(capability) => format!("this call is not granted \"{}\"", capability.label()),
                None => "this extension is not enabled".into(),
            };
            return Err(failure(HostErrorCode::Denied, &message));
        }
        match request {
            HostRequest::SkillList => {
                self.skill_reply(crate::skill_tool::LIST, serde_json::Map::new())
                    .await
            }
            HostRequest::SkillRead { name, path } => {
                let mut args = serde_json::Map::new();
                args.insert("name".into(), serde_json::Value::String(name));
                if let Some(path) = path {
                    args.insert("path".into(), serde_json::Value::String(path));
                }
                self.skill_reply(crate::skill_tool::INFO, args).await
            }
            HostRequest::ReadFile { path } => {
                let root = self.workspace()?;
                blocking(move || workspace::read_file(root, &path)).await
            }
            HostRequest::ListDirectory { path } => {
                let root = self.workspace()?;
                blocking(move || workspace::list_directory(root, &path)).await
            }
            HostRequest::Search {
                path,
                query,
                max_matches,
            } => {
                let root = self.workspace()?;
                blocking(move || workspace::search(root, &path, &query, max_matches)).await
            }
            HostRequest::SearchPattern {
                path,
                pattern,
                regex,
                ignore_case,
                include,
                max_matches,
            } => {
                let root = self.workspace()?;
                blocking(move || {
                    let options = workspace::Pattern {
                        regex,
                        ignore_case,
                        include: include.as_deref(),
                    };
                    workspace::search_pattern(root, &path, &pattern, options, max_matches)
                })
                .await
            }
            HostRequest::Find {
                path,
                glob,
                max_results,
            } => {
                let root = self.workspace()?;
                blocking(move || workspace::find(root, &path, &glob, max_results)).await
            }
            HostRequest::WriteFile {
                path,
                contents,
                overwrite,
            } => {
                let root = self.workspace()?;
                blocking(move || workspace::write_file(root, &path, &contents, overwrite)).await
            }
            HostRequest::EditFile { path, old, new } => {
                let root = self.workspace()?;
                blocking(move || workspace::edit_file(root, &path, &old, &new)).await
            }
            HostRequest::PatchFiles { files, check_only } => {
                let root = self.workspace()?;
                blocking(move || workspace::patch_files(root, &files, check_only)).await
            }
            HostRequest::CreateDirectory { path } => {
                let root = self.workspace()?;
                blocking(move || workspace::create_directory(root, &path)).await
            }
            HostRequest::GitStatus => {
                let root = self.workspace()?;
                git_call(move |interrupt| git::status(root, interrupt)).await
            }
            HostRequest::GitLog { max } => {
                let root = self.workspace()?;
                git_call(move |interrupt| git::log(root, max, &interrupt)).await
            }
            HostRequest::GitDiff { path, staged } => {
                let root = self.workspace()?;
                git_call(move |interrupt| git::diff(root, &path, staged, &interrupt)).await
            }
            HostRequest::GitCommit { message, paths } => {
                let root = self.workspace()?;
                let user = self.user_git;
                // Stop early enough that a commit made at the last moment
                // still reaches the guest before the invocation is dropped.
                let budget = self
                    .deadline
                    .saturating_duration_since(tokio::time::Instant::now())
                    .saturating_sub(GIT_WRITE_RESERVE)
                    .min(GIT_TIMEOUT);
                if budget.is_zero() {
                    return Err(failure(
                        HostErrorCode::Budget,
                        "too little time is left in this call to commit",
                    ));
                }
                git_write_call_with_timeout(
                    move |interrupt| {
                        git_commit::commit(root, &message, &paths, &user()?, &interrupt)
                    },
                    budget,
                )
                .await
            }
            HostRequest::Fetch { url } => fetch::fetch(self.net.as_ref(), &self.hosts, &url).await,
            HostRequest::StateGet { key } => {
                check_key(&key)?;
                let value = self
                    .lock()?
                    .extension_state(&self.extension, &key)
                    .map_err(|e| state_failure(&e))?;
                Ok(HostReply::State(value.map(Into::into)))
            }
            HostRequest::StatePut { key, value } => {
                check_key(&key)?;
                let put = self
                    .lock()?
                    .put_extension_state(
                        &self.extension,
                        &key,
                        value.as_deref().map(|v| v.as_slice()),
                        STATE_QUOTA,
                    )
                    .map_err(|e| state_failure(&e))?;
                match put {
                    StatePut::Stored => Ok(HostReply::Done),
                    StatePut::Quota => Err(failure(
                        HostErrorCode::Budget,
                        "extension state quota is full",
                    )),
                }
            }
            HostRequest::Log { level, message } => {
                self.logs += 1;
                if self.logs > MAX_LOGS {
                    return Err(failure(HostErrorCode::Budget, "log budget is spent"));
                }
                let message = bounded(&message, MAX_LOG_MESSAGE);
                let extension = self.extension.as_str();
                match level {
                    LogLevel::Debug => tracing::debug!(extension, message, "extension log"),
                    LogLevel::Info => tracing::info!(extension, message, "extension log"),
                    LogLevel::Warn => tracing::warn!(extension, message, "extension log"),
                }
                Ok(HostReply::Done)
            }
        }
    }
    fn workspace(&self) -> Result<PathBuf, HostFailure> {
        self.workspace
            .clone()
            .ok_or_else(|| failure(HostErrorCode::NoWorkspace, "this call has no workspace"))
    }
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Store>, HostFailure> {
        self.store
            .lock()
            .map_err(|_| failure(HostErrorCode::Io, "extension state is unavailable"))
    }
}

/// A store failure is the gateway's problem, not the guest's: record it here,
/// where it is handled, and give the guest only a generic code.
fn state_failure(error: &StoreError) -> HostFailure {
    crate::error::handle("extension_state", error);
    failure(HostErrorCode::Io, "extension state is unavailable")
}
fn check_key(key: &str) -> Result<(), HostFailure> {
    let valid = (1..=MAX_KEY).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    if valid {
        Ok(())
    } else {
        Err(failure(HostErrorCode::Invalid, "invalid state key"))
    }
}
fn bounded(text: &str, limit: usize) -> &str {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

async fn blocking(
    work: impl FnOnce() -> Result<HostReply, HostFailure> + Send + 'static,
) -> Result<HostReply, HostFailure> {
    joined_reply(tokio::task::spawn_blocking(work).await)
}
fn joined_reply(
    joined: Result<Result<HostReply, HostFailure>, tokio::task::JoinError>,
) -> Result<HostReply, HostFailure> {
    joined.unwrap_or_else(|error| {
        crate::error::handle("extension_host_call", &error);
        Err(failure(HostErrorCode::Io, "host call failed"))
    })
}
/// Run a blocking git call that stops at `GIT_TIMEOUT` or when this future
/// is dropped (the invocation timed out or was revoked). A blocking task
/// cannot be aborted, so the interrupt flag is how it learns to end; on
/// timeout the flag is raised and the call awaited until it returns.
async fn git_call(
    work: impl FnOnce(Arc<std::sync::atomic::AtomicBool>) -> Result<HostReply, HostFailure>
    + Send
    + 'static,
) -> Result<HostReply, HostFailure> {
    git_call_with_timeout(work, GIT_TIMEOUT).await
}
async fn git_call_with_timeout(
    work: impl FnOnce(Arc<std::sync::atomic::AtomicBool>) -> Result<HostReply, HostFailure>
    + Send
    + 'static,
    timeout: Duration,
) -> Result<HostReply, HostFailure> {
    run_git(work, timeout, Late::Refuse).await
}
/// Like [`git_call`], for a call that changes the repository, stopping at
/// `timeout`. Once past its point of no return it finishes, and its outcome
/// is reported as it is: a commit made after the deadline is still a commit.
/// If this future is dropped, a call that has not reached that point stops
/// without changing anything.
async fn git_write_call_with_timeout(
    work: impl FnOnce(Arc<std::sync::atomic::AtomicBool>) -> Result<HostReply, HostFailure>
    + Send
    + 'static,
    timeout: Duration,
) -> Result<HostReply, HostFailure> {
    run_git(work, timeout, Late::Report).await
}
/// What a git call that returns after its deadline reports.
#[derive(Clone, Copy)]
enum Late {
    /// A read: the deadline has expired, whatever the result.
    Refuse,
    /// A write: its own outcome, which says whether it changed anything.
    Report,
}
async fn run_git(
    work: impl FnOnce(Arc<std::sync::atomic::AtomicBool>) -> Result<HostReply, HostFailure>
    + Send
    + 'static,
    timeout: Duration,
    late: Late,
) -> Result<HostReply, HostFailure> {
    let interrupt = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop = Interrupt(Arc::clone(&interrupt));
    let mut call = tokio::task::spawn_blocking(move || work(interrupt));
    match tokio::time::timeout(timeout, &mut call).await {
        Ok(joined) => joined_reply(joined),
        Err(_) => {
            stop.set();
            let completed = joined_reply(call.await);
            match late {
                Late::Report => completed,
                // Joined for cleanup and diagnostics, but a late success
                // cannot undo the deadline that has already expired.
                Late::Refuse => Err(failure(HostErrorCode::Budget, "the git call took too long")),
            }
        }
    }
}
/// Raises a scan's interrupt flag when set or dropped.
struct Interrupt(Arc<std::sync::atomic::AtomicBool>);
impl Interrupt {
    fn set(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}
impl Drop for Interrupt {
    fn drop(&mut self) {
        self.set();
    }
}
mod fetch;
mod git;
mod git_commit;
pub mod packages;
mod patch;
mod proposals;
mod workspace;
pub(crate) use workspace::read_limited;

#[cfg(test)]
mod tests;
