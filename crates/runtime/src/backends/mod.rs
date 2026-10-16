//! External agent runtimes never inherit ambient configuration or credentials.
mod claude;
mod codex;
mod isolation;
pub(crate) mod proxy;
mod rpc;
use crate::error::check;
use a206_core::*;
use std::path::Path;
use tokio::sync::mpsc;

/// Why an external coding backend could not be configured, launched or run.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("Agent 206 does not need an external runtime")]
    Native,
    #[error("backend paths must be absolute and contain no control characters")]
    Path,
    #[error("Claude Agent needs a JavaScript ACP adapter entry point; Codex does not")]
    Adapter,
    #[error("wrong coding backend configuration")]
    WrongBackend,
    #[error(
        "coding backends require the isolated credential broker for authenticated providers; it is not connected yet"
    )]
    Credentialed,
    #[error("selected provider protocol is incompatible with this coding backend")]
    Protocol,
    #[error(
        "external backend delegation is unavailable until per-child backend policy is implemented"
    )]
    Delegation,
    #[error("backend model metadata does not match its provider selection")]
    ModelMismatch,
    #[error("model not selected")]
    NoModel,
    #[error("backend transcript exceeds its input limit")]
    TranscriptTooLarge,
    #[error("{what} is missing or inaccessible")]
    Missing {
        what: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}")]
    Placement(&'static str),
    #[error("could not prepare the backend sandbox")]
    Setup(#[source] std::io::Error),
    #[error("could not write the backend configuration")]
    Configuration(#[source] toml::ser::Error),
    #[error("backend paths must be UTF-8")]
    NonUtf8Path,
    #[error(
        "external coding backends are disabled on this platform until equivalent OS filesystem and network isolation is implemented"
    )]
    Unsupported,
    #[error("could not start isolated backend")]
    Spawn(#[source] std::io::Error),
    #[error("could not locate this executable to run the sandbox helper")]
    Helper(#[source] std::io::Error),
    #[error("could not prepare the backend's OS isolation")]
    Sandbox(#[from] a206_sandbox::SandboxError),
    #[error("the sandbox could not start the backend")]
    SandboxFailed(#[source] ErrorReport),
    #[error("the sandbox exited ({status}) before the backend started: {diagnostic}")]
    SandboxExited {
        status: std::process::ExitStatus,
        /// Bubblewrap's own explanation, as it printed it.
        diagnostic: String,
    },
    #[error("could not read the sandbox helper's report")]
    SandboxReport(#[source] std::io::Error),
    #[error("the sandbox helper sent a malformed report")]
    SandboxMalformed(#[source] rmp_serde::decode::Error),
    #[error("the sandbox helper speaks version {version}, not this gateway's")]
    SandboxVersion { version: u32 },
    #[error("the sandbox did not report readiness in time")]
    SandboxTimeout(#[source] tokio::time::error::Elapsed),
    #[error("backend has no usable process ID")]
    ProcessId,
    #[error("could not start the backend's credential proxy")]
    Proxy(#[source] std::io::Error),
    #[error("backend protocol connection failed")]
    Io(#[source] std::io::Error),
    #[error("backend closed its protocol connection")]
    Closed,
    #[error("backend exceeded its protocol output limit")]
    OutputLimit,
    #[error("backend sent invalid JSON")]
    Json(#[source] serde_json::Error),
    #[error("{0}")]
    Malformed(&'static str),
    #[error("Codex rejected {method}")]
    Rejected { method: &'static str },
    #[error("{backend} did not complete this turn; inspect the backend activity")]
    Incomplete { backend: &'static str },
    #[error("{backend} completed without a text response")]
    Empty { backend: &'static str },
    #[error("backend message exceeds the conversation limit")]
    MessageTooLarge,
    #[error("Claude Agent ACP session failed")]
    Acp(#[source] agent_client_protocol::Error),
    #[error("coding backend exceeded its ten-minute run limit")]
    Timeout(#[source] tokio::time::error::Elapsed),
    #[error("coding backend cancelled")]
    Cancelled,
    #[error("backend output lock poisoned")]
    Poisoned,
}
impl crate::error::Classified for BackendError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::Native
            | Self::Path
            | Self::Adapter
            | Self::WrongBackend
            | Self::Protocol
            | Self::ModelMismatch
            | Self::NoModel
            | Self::TranscriptTooLarge
            | Self::Placement(_)
            | Self::NonUtf8Path => ErrorKind::InvalidRequest,
            // Absent and unreadable paths need different recovery; the I/O cause decides.
            Self::Missing { .. } => return None,
            Self::Credentialed
            | Self::Delegation
            | Self::Unsupported
            | Self::Spawn(_)
            | Self::SandboxExited { .. } => ErrorKind::Unavailable,
            // The sandbox's own error or report knows its kind.
            Self::Sandbox(_) | Self::SandboxFailed(_) => return None,
            Self::SandboxReport(_) | Self::SandboxMalformed(_) | Self::SandboxVersion { .. } => {
                ErrorKind::Protocol
            }
            Self::SandboxTimeout(_) => ErrorKind::Timeout,
            Self::Helper(_) => ErrorKind::Internal,
            Self::Setup(_)
            | Self::Configuration(_)
            | Self::ProcessId
            | Self::Proxy(_)
            | Self::Poisoned => ErrorKind::Internal,
            Self::Io(_)
            | Self::Closed
            | Self::OutputLimit
            | Self::Json(_)
            | Self::Malformed(_)
            | Self::Rejected { .. }
            | Self::Acp(_) => ErrorKind::Protocol,
            Self::Incomplete { .. } | Self::Empty { .. } | Self::MessageTooLarge => {
                ErrorKind::Provider
            }
            Self::Timeout(_) => ErrorKind::Timeout,
            Self::Cancelled => ErrorKind::Cancelled,
        })
    }
}
type Result<T, E = BackendError> = std::result::Result<T, E>;
pub enum Progress {
    Text(String),
    Activity(String),
}
pub fn validate_config(config: &BackendConfig) -> Result<()> {
    check(config.kind != CodingBackend::Native, BackendError::Native)?;
    for path in [&config.executable, &config.runtime_root]
        .into_iter()
        .chain(config.adapter_script.iter())
    {
        check(
            Path::new(path).is_absolute() && !path.chars().any(char::is_control),
            BackendError::Path,
        )?;
    }
    check(
        (config.kind == CodingBackend::ClaudeAgent) == config.adapter_script.is_some(),
        BackendError::Adapter,
    )?;
    Ok(())
}
pub fn validate_selection(
    config: &ProviderConfig,
    thread: &Thread,
    backend: &BackendConfig,
) -> Result<()> {
    validate_config(backend)?;
    check(backend.kind == thread.backend, BackendError::WrongBackend)?;
    check(config.credential.is_none(), BackendError::Credentialed)?;
    check(
        match thread.backend {
            CodingBackend::Codex => config.api == ProviderApi::OpenAi,
            CodingBackend::ClaudeAgent => config.api == ProviderApi::Anthropic,
            CodingBackend::Native => false,
        },
        BackendError::Protocol,
    )?;
    check(thread.parent.is_none(), BackendError::Delegation)?;
    Ok(())
}
pub struct Run<'a> {
    pub config: &'a ProviderConfig,
    pub thread: &'a Thread,
    pub prompt: &'a str,
    pub info: &'a ModelInfo,
    pub backend: &'a BackendConfig,
    pub workspace: Option<&'a Path>,
    pub state_root: &'a Path,
    pub progress: mpsc::Sender<Progress>,
    pub cancel: tokio_util::sync::CancellationToken,
}
pub async fn complete(run: Run<'_>) -> Result<(String, Usage)> {
    let Run {
        config,
        thread,
        prompt,
        info,
        backend,
        workspace,
        state_root,
        progress,
        cancel,
    } = run;
    validate_selection(config, thread, backend)?;
    check(
        thread.model.as_ref() == Some(&info.key) && info.key.provider == config.id,
        BackendError::ModelMismatch,
    )?;
    let mut input = transcript(thread)?;
    let proxy = proxy::Proxy::start(config.clone(), info.clone())
        .await
        .map_err(BackendError::Proxy)?;
    let mut process = isolation::Process::launch(
        backend,
        thread,
        workspace,
        state_root,
        proxy.port(),
        proxy.token(),
    )
    .await?;
    if let Some(workspace) = &process.workspace {
        input.insert_str(
            0,
            &format!(
                "Assigned workspace: {}\nUse this directory for project work.\n",
                workspace.display()
            ),
        );
    }
    let result = {
        let operation = async {
            match thread.backend {
                CodingBackend::Codex => {
                    codex::run(&mut process, thread, prompt, &input, progress).await
                }
                CodingBackend::ClaudeAgent => {
                    claude::run(&mut process, thread, prompt, &input, progress).await
                }
                CodingBackend::Native => Err(BackendError::Native),
            }
        };
        tokio::select! {
            result = tokio::time::timeout(std::time::Duration::from_secs(600), operation) => result.map_err(BackendError::Timeout).and_then(|result| result),
            _ = cancel.cancelled() => Err(BackendError::Cancelled),
        }
    };
    drop(proxy);
    process.stop().await;
    result
}
fn transcript(thread: &Thread) -> Result<String> {
    let mut text = String::new();
    for message in &thread.messages {
        if matches!(message.role, Role::User | Role::Assistant) {
            text.push_str(if message.role == Role::User {
                "\nUSER:\n"
            } else {
                "\nASSISTANT:\n"
            });
            text.push_str(&crate::provider::message_content(message));
            check(
                text.len() <= 2 * 1024 * 1024,
                BackendError::TranscriptTooLarge,
            )?;
        }
    }
    Ok(text)
}
