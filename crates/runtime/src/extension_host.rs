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
    Capability, ErrorKind, ErrorReport, PluginSummary,
    extension::{Args, ToolOutput, ToolSpec, UiEvent, View},
};
use a206_extensions::{
    ArgError, Limits, Manifest, ManifestError,
    abi::{
        ABI, Call, DirEntry, EntryKind, HostErrorCode, HostFailure, HostReply, HostRequest,
        Invocation, LogLevel, Outcome,
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
            Self::NoProject => ErrorKind::InvalidRequest,
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
            HostErrorCode::Budget => ErrorKind::Conflict,
            HostErrorCode::Unsupported | HostErrorCode::Other => return None,
            HostErrorCode::Io => ErrorKind::Storage,
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

#[derive(Clone)]
pub struct ExtensionHost {
    store: Arc<Mutex<Store>>,
    /// Passed to unconfined workers; a Linux worker gets an empty one instead.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    data_dir: PathBuf,
}
impl ExtensionHost {
    pub fn new(store: Arc<Mutex<Store>>, data_dir: PathBuf) -> Self {
        Self { store, data_dir }
    }

    /// Tools of enabled extensions whose declared capabilities the user has
    /// granted. A caller still checks its own ceiling with [`Self::call_tool`].
    pub fn tools(&self) -> Result<Vec<ExtensionTool>, ExtensionError> {
        Ok(self
            .plugins()?
            .into_iter()
            .filter(|plugin| plugin.enabled)
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

    /// Run a tool. Arguments are checked against its declaration first.
    pub async fn call_tool(
        &self,
        extension: &str,
        tool: &str,
        args: Args,
        scope: Scope,
    ) -> Result<ToolOutput, ExtensionError> {
        let manifest = self.manifest(extension)?;
        let spec = manifest.tool(tool)?;
        a206_extensions::check_args(spec, &args)?;
        let call = Call::Tool {
            name: tool.into(),
            args,
        };
        match self
            .invoke(&manifest, &spec.capabilities, call, scope)
            .await?
        {
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
        let manifest = self.manifest(extension)?;
        let spec = manifest.panel(panel)?;
        let call = Call::Panel {
            id: panel.into(),
            event,
        };
        match self
            .invoke(&manifest, &spec.capabilities, call, scope)
            .await?
        {
            Outcome::Panel(view) => Ok(view),
            _ => Err(ExtensionError::Unexpected),
        }
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
    fn manifest(&self, extension: &str) -> Result<Manifest, ExtensionError> {
        a206_extensions::builtin(extension)
            .map_err(ExtensionError::Builtins)?
            .map(|builtin| builtin.manifest)
            .ok_or_else(|| ExtensionError::NotInstalled(extension.into()))
    }

    async fn invoke(
        &self,
        manifest: &Manifest,
        declared: &BTreeSet<Capability>,
        call: Call,
        scope: Scope,
    ) -> Result<Outcome, ExtensionError> {
        let plugin = self
            .plugins()?
            .into_iter()
            .find(|p| p.id == manifest.id)
            .ok_or_else(|| ExtensionError::NotInstalled(manifest.id.clone()))?;
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
        let workspace_capabilities = [Capability::ReadWorkspace, Capability::WriteWorkspace];
        if scope.workspace.is_none() && workspace_capabilities.iter().any(|c| declared.contains(c))
        {
            return Err(ExtensionError::NoProject);
        }
        let host = HostCalls {
            extension: manifest.id.clone(),
            effective: declared.clone(),
            workspace: scope.workspace,
            store: Arc::clone(&self.store),
            logs: 0,
            last_failure: None,
        };
        let start = Start {
            extension: manifest.id.clone(),
            invocation: Invocation { abi: ABI, call },
            limits: Limits::default(),
        };
        tokio::time::timeout(TIMEOUT, self.run_worker(start, host))
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
        write_frame(&mut input, &start).await?;
        let result = loop {
            match read_frame::<Frame>(&mut output).await? {
                Frame::Host(request) => {
                    let reply = host.handle(request).await;
                    write_frame(&mut input, &reply).await?;
                }
                Frame::Done(result) => break result,
            }
        };
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

/// A running worker, confined on Linux.
#[cfg(target_os = "linux")]
struct Worker(crate::sandbox::Confined);
#[cfg(not(target_os = "linux"))]
struct Worker(tokio::process::Child);
impl Worker {
    fn child(&mut self) -> &mut tokio::process::Child {
        #[cfg(target_os = "linux")]
        return &mut self.0.child;
        #[cfg(not(target_os = "linux"))]
        return &mut self.0;
    }
    async fn stop(self) {
        #[cfg(target_os = "linux")]
        self.0.stop().await;
    }
}

impl ExtensionHost {
    #[cfg(not(target_os = "linux"))]
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
    #[cfg(target_os = "linux")]
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
    extension: String,
    effective: BTreeSet<Capability>,
    workspace: Option<PathBuf>,
    store: Arc<Mutex<Store>>,
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
    async fn execute(&mut self, request: HostRequest) -> Result<HostReply, HostFailure> {
        // The stored plugin is read again for every call, so disabling or
        // revoking takes effect inside an invocation already running.
        let required = a206_extensions::required_capability(&request);
        let allowed = self.lock()?.state.plugins.iter().any(|p| {
            p.id == self.extension
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
            HostRequest::ReadFile { path } => {
                let root = self.workspace()?;
                blocking(move || read_file(root, &path)).await
            }
            HostRequest::ListDirectory { path } => {
                let root = self.workspace()?;
                blocking(move || list_directory(root, &path)).await
            }
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
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|error| {
            crate::error::handle("extension_host_call", &error);
            Err(failure(HostErrorCode::Io, "host call failed"))
        })
}
/// Host messages never include host paths: the guest sees only its own request.
fn io_failure(error: &std::io::Error) -> HostFailure {
    use std::io::ErrorKind as Io;
    match error.kind() {
        Io::NotFound => failure(HostErrorCode::NotFound, "no such file or directory"),
        // cap-std refuses absolute paths, `..` escapes and symlinks that
        // leave the workspace with PermissionDenied.
        Io::PermissionDenied => failure(
            HostErrorCode::Invalid,
            "the path is outside the workspace or not readable",
        ),
        Io::InvalidInput => failure(HostErrorCode::Invalid, "invalid path"),
        _ => failure(HostErrorCode::Io, "the workspace could not be read"),
    }
}
fn open_workspace(root: PathBuf) -> Result<cap_std::fs::Dir, HostFailure> {
    cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority())
        .map_err(|e| io_failure(&e))
}
fn read_file(root: PathBuf, path: &str) -> Result<HostReply, HostFailure> {
    use std::io::Read;
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true);
    // Open without blocking: opening a FIFO for reading otherwise waits for
    // a writer forever. A regular file reads the same either way.
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits());
    }
    let file = open_workspace(root)?
        .open_with(path, &options)
        .map_err(|e| io_failure(&e))?;
    if !file.metadata().map_err(|e| io_failure(&e))?.is_file() {
        return Err(failure(HostErrorCode::Invalid, "not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io_failure(&e))?;
    if bytes.len() as u64 > MAX_FILE {
        return Err(failure(HostErrorCode::TooLarge, "the file exceeds 512 KiB"));
    }
    Ok(HostReply::File(bytes.into()))
}
fn list_directory(root: PathBuf, path: &str) -> Result<HostReply, HostFailure> {
    let directory = open_workspace(root)?;
    let directory = if path.is_empty() || path == "." {
        directory
    } else {
        directory.open_dir(path).map_err(|e| io_failure(&e))?
    };
    let mut entries = Vec::new();
    for entry in directory.entries().map_err(|e| io_failure(&e))? {
        let entry = entry.map_err(|e| io_failure(&e))?;
        if entries.len() == MAX_ENTRIES {
            return Err(failure(
                HostErrorCode::TooLarge,
                "the directory has more than 1024 entries",
            ));
        }
        let kind = match entry.file_type().map_err(|e| io_failure(&e))? {
            kind if kind.is_dir() => EntryKind::Directory,
            kind if kind.is_symlink() => EntryKind::Symlink,
            kind if kind.is_file() => EntryKind::File,
            _ => EntryKind::Other,
        };
        // A size that cannot be read is reported as unknown, never as zero.
        let size = (kind == EntryKind::File)
            .then(|| entry.metadata().ok().map(|m| m.len()))
            .flatten();
        entries.push(DirEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            kind,
            size,
        });
    }
    Ok(HostReply::Directory(entries))
}

#[cfg(test)]
mod tests;
