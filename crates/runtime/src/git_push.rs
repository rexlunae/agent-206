//! Pushing a branch of the project's repository for the native run engine:
//! a [`ToolHost`] whose `git_push` sends a local branch to the branch of the
//! same name on one of the repository's HTTPS remotes, as `git push` does.
//!
//! A push leaves the computer, so the tool is denied until the user permits
//! it on the conversation, needs the conversation's GitWrite grant, and each
//! call still waits for the user's approval. The forge's token stays in the
//! vault: the user names, per forge origin, which vault credential pushes
//! send, and the broker attaches it only for the origin it is bound to. Git
//! itself never runs; only fast-forward updates of one branch are sent, as
//! a pack of whole objects that must fit one broker message.
//!
//! Cancelling a call is not always safe: once the pack is sent, the server
//! may update its branch even though the call reports nothing.
use crate::{
    extension_host::git_push::{self as repository, PushTarget},
    gateway::Pushes,
    tools::{ToolHost, ToolListing, ToolOutput, ToolRequest, ToolScope, ToolSpec},
};
use a206_core::{Capability, ErrorKind, ErrorReport, ToolPermission};
use a206_extensions::abi::{HostErrorCode, HostFailure};
use a206_git_push::{Limits, PackError, Update, WireError};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use std::{path::PathBuf, sync::atomic::AtomicBool};

/// Pushes a branch to a remote.
pub const PUSH: &str = "git_push";

/// The most one push sends: what fits one broker message, less room for the
/// update command.
pub const LIMITS: Limits = Limits {
    commits: 10_000,
    objects: 100_000,
    bytes: a206_broker::MAX_PROVIDER_BODY - 4096,
};

/// Why a push was refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum PushError {
    #[error("`{0}` has the wrong type")]
    Type(&'static str),
    #[error("unknown argument `{0}`")]
    Unknown(String),
    #[error("unknown push tool `{0}`")]
    UnknownTool(String),
    #[error("the user did not approve this push")]
    NotApproved,
    #[error("the conversation has no project to push from")]
    NoProject,
    #[error(transparent)]
    Repository(HostFailure),
    #[error("the branch name is not one git accepts")]
    BranchName,
    #[error("there is no local branch named {0}")]
    NoBranch(String),
    #[error("remote names may hold only letters, digits, `.`, `_` and `-`")]
    RemoteName,
    #[error("the repository has no remote named {0}")]
    NoRemote(String),
    #[error(
        "only HTTPS remotes can be pushed; the remote's URL is not an HTTPS repository URL without a query or fragment"
    )]
    RemoteUrl,
    #[error(
        "the remote's URL carries a username or password; remove it, the vault supplies the credential"
    )]
    RemoteCredentials,
    #[error(
        "the remote's URL is rewritten by a url.*.insteadOf setting, which this push does not apply"
    )]
    Rewritten,
    #[error(
        "no git credential is set for {origin}; add one in Settings, Git credentials, and bind the vault credential to {origin}"
    )]
    NoCredential { origin: String },
    #[error("the vault broker could not be reached")]
    Vault(#[source] a206_broker::ClientError),
    #[error("the vault broker failed the push")]
    Broker(#[source] a206_broker::ClientError),
    #[error("the vault is not unlocked for the gateway; unlock it on a trusted client")]
    NotPaired,
    #[error("the running vault broker is too old to push; restart it with `agent206 vault stop`")]
    BrokerTooOld,
    #[error("the vault broker sent an unexpected reply")]
    UnexpectedReply,
    #[error(
        "the server refused the credential (HTTP {status}); check that the token is valid and may push to this repository"
    )]
    Refused { status: u16 },
    #[error("the server has no repository at the remote's URL, or the credential cannot see it")]
    NoRepository,
    #[error("the server answered HTTP {status}")]
    Status { status: u16 },
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error(transparent)]
    Pack(#[from] PackError),
    #[error("{0} changed while the push was prepared; try again")]
    Moved(String),
    #[error(
        "the server's {0} has commits this branch does not; fetch and merge them, then push again"
    )]
    NotFastForward(String),
    #[error("the branch's history is too long to check that the push is a fast-forward")]
    History,
    #[error("the server could not store the pushed objects: {0}")]
    Unpack(String),
    #[error("the server refused the update of {branch}: {reason}")]
    Rejected { branch: String, reason: String },
    #[error("the server did not report whether it updated {0}")]
    Unreported(String),
    #[error(
        "pushed {branch}, but the remote-tracking branch {remote}/{branch} could not be updated"
    )]
    Tracking {
        branch: String,
        remote: String,
        #[source]
        cause: HostFailure,
    },
    #[error("the gateway's state could not be read")]
    Gateway(#[source] crate::gateway::GatewayError),
    #[error("the repository work stopped unexpectedly")]
    Task(#[source] tokio::task::JoinError),
}
impl crate::error::Classified for PushError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::Type(_) | Self::Unknown(_) | Self::BranchName | Self::RemoteName => {
                ErrorKind::InvalidRequest
            }
            Self::UnknownTool(_) | Self::NoBranch(_) | Self::NoRemote(_) | Self::NoRepository => {
                ErrorKind::NotFound
            }
            Self::NotApproved | Self::NoCredential { .. } | Self::Refused { .. } => {
                ErrorKind::PermissionDenied
            }
            Self::NoProject
            | Self::RemoteUrl
            | Self::RemoteCredentials
            | Self::Rewritten
            | Self::History => ErrorKind::InvalidRequest,
            Self::Repository(failure) => match failure.code {
                HostErrorCode::NotFound | HostErrorCode::NoWorkspace => ErrorKind::NotFound,
                HostErrorCode::Conflict | HostErrorCode::Exists => ErrorKind::Conflict,
                HostErrorCode::Denied => ErrorKind::PermissionDenied,
                HostErrorCode::Io => ErrorKind::Storage,
                HostErrorCode::Timeout | HostErrorCode::Budget => ErrorKind::Timeout,
                _ => ErrorKind::InvalidRequest,
            },
            Self::Vault(_) | Self::NotPaired | Self::BrokerTooOld => ErrorKind::Unavailable,
            Self::Broker(_) | Self::Gateway(_) => return None,
            Self::UnexpectedReply | Self::Wire(_) => ErrorKind::Protocol,
            Self::Status { .. } | Self::Unpack(_) | Self::Unreported(_) => ErrorKind::Provider,
            Self::Pack(PackError::Git(_) | PackError::Compress(_)) => ErrorKind::Storage,
            Self::Pack(_) => ErrorKind::InvalidRequest,
            Self::Moved(_) | Self::NotFastForward(_) | Self::Rejected { .. } => ErrorKind::Conflict,
            Self::Tracking { .. } => ErrorKind::Storage,
            Self::Task(_) => ErrorKind::Internal,
        })
    }
}
fn fail(error: PushError) -> ErrorReport {
    crate::error::report(&error)
}

/// Offers `git_push` in conversations with a project, once the user has
/// set a git credential for at least one forge.
pub struct PushHost {
    pushes: Pushes,
}
impl PushHost {
    pub fn new(pushes: Pushes) -> Self {
        Self { pushes }
    }

    fn spec() -> ToolSpec {
        ToolSpec {
            name: PUSH.into(),
            description: "Push a local branch of the project's git repository to the branch of the same name on one of its HTTPS remotes, like `git push <remote> <branch>`. Only a fast-forward update is sent: if the remote branch has commits this branch lacks, the push is refused. The user approves each call. Refused when the repository has a pre-push hook or the remote's URL is not HTTPS.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "branch": {"type": "string", "description": "The local branch to push, such as feature/login. Defaults to the current branch."},
                    "remote": {"type": "string", "description": "The remote to push to. Defaults to the branch's configured remote, else origin."}
                },
                "additionalProperties": false
            }),
            capabilities: [Capability::GitWrite].into(),
            permission: ToolPermission::Ask,
            whole_output: false,
        }
    }

    async fn run(&self, scope: &ToolScope, request: &ToolRequest) -> Result<Value, PushError> {
        if request.name != PUSH {
            return Err(PushError::UnknownTool(request.name.clone()));
        }
        // The gateway asks the user first; never push on a call it did not.
        if !request.approved {
            return Err(PushError::NotApproved);
        }
        let args = &request.arguments;
        if let Some(key) = args
            .keys()
            .find(|key| !matches!(key.as_str(), "branch" | "remote"))
        {
            return Err(PushError::Unknown(key.clone()));
        }
        let branch = text(args, "branch")?;
        let remote = text(args, "remote")?;
        let root = scope.workspace.clone().ok_or(PushError::NoProject)?;
        let target = blocking({
            let root = root.clone();
            move || {
                let never = AtomicBool::new(false);
                repository::target(root, branch.as_deref(), remote.as_deref(), &never)
            }
        })
        .await?;
        let origin = target.url.origin().ascii_serialization();
        let credential = self
            .pushes
            .credential(&origin)
            .map_err(PushError::Gateway)?
            .ok_or_else(|| PushError::NoCredential {
                origin: origin.clone(),
            })?;
        let (broker_root, pairing) = self.pushes.broker().map_err(PushError::Gateway)?;
        let mut broker = Broker::connect(&broker_root, pairing.as_ref()).await?;
        let call = |request| a206_broker::GitCall {
            credential: credential.credential.clone(),
            username: credential.username.clone(),
            repository: target.url.to_string(),
            request,
        };
        let advertisement = broker.call(call(None)).await?;
        let advertised = a206_git_push::parse_advertisement(&advertisement, target.hash)?;
        let packed = blocking({
            let target = target.clone();
            move || {
                let never = AtomicBool::new(false);
                repository::pack(root, &target, &advertised, LIMITS, &never)
                    .map(|packed| (packed, advertised))
            }
        })
        .await?;
        let (Some(packed), advertised) = packed else {
            return Ok(json!({
                "branch": target.branch,
                "remote": target.remote,
                "commit": target.tip.to_string(),
                "up_to_date": true,
            }));
        };
        let update = Update {
            old: packed.old,
            new: target.tip,
            name: target.reference(),
        };
        let body = update.request(&advertised, &packed.pack)?;
        let answer = broker.call(call(Some(body))).await?;
        let report = a206_git_push::parse_report(&answer, &update.name)?;
        report.unpack.map_err(PushError::Unpack)?;
        match report.reference {
            Some(Ok(())) => {}
            Some(Err(reason)) => {
                return Err(PushError::Rejected {
                    branch: target.branch,
                    reason,
                });
            }
            None => return Err(PushError::Unreported(target.branch)),
        }
        tracing::info!(
            commit = %target.tip,
            commits = packed.commits,
            objects = packed.objects,
            "git_push updated a remote branch"
        );
        let workspace = scope.workspace.clone().ok_or(PushError::NoProject)?;
        record(workspace, &target).await?;
        Ok(json!({
            "branch": target.branch,
            "remote": target.remote,
            "from": (!packed.old.is_null()).then(|| packed.old.to_string()),
            "commit": target.tip.to_string(),
            "commits": packed.commits,
            "objects": packed.objects,
            "bytes": packed.pack.len(),
        }))
    }
}

/// Update the remote-tracking branch; the push itself already happened.
async fn record(root: PathBuf, target: &PushTarget) -> Result<(), PushError> {
    let recorded = {
        let target = target.clone();
        tokio::task::spawn_blocking(move || repository::record(root, &target))
            .await
            .map_err(PushError::Task)?
    };
    recorded.map_err(|cause| PushError::Tracking {
        branch: target.branch.clone(),
        remote: target.remote.clone(),
        cause,
    })
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, PushError> + Send + 'static,
) -> Result<T, PushError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(PushError::Task)?
}

fn text(args: &Map<String, Value>, name: &'static str) -> Result<Option<String>, PushError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(PushError::Type(name)),
    }
}

/// A paired gateway connection to the vault broker that can push.
struct Broker {
    #[cfg(unix)]
    client: a206_broker::Client,
}
impl Broker {
    #[cfg(unix)]
    async fn connect(
        root: &std::path::Path,
        pairing: Option<&a206_core::BrokerPairing>,
    ) -> Result<Self, PushError> {
        use a206_broker::{Request, Role, WireSecret};
        let pairing = pairing.ok_or(PushError::NotPaired)?;
        let mut client = a206_broker::Client::connect(root, Role::Gateway)
            .await
            .map_err(PushError::Vault)?;
        if client.version() < a206_broker::GIT_VERSION {
            return Err(PushError::BrokerTooOld);
        }
        client
            .call(Request::GatewayAuth {
                token: WireSecret::new(pairing.expose().to_owned()),
            })
            .await
            .map_err(PushError::Broker)?;
        Ok(Self { client })
    }
    #[cfg(not(unix))]
    async fn connect(
        _root: &std::path::Path,
        _pairing: Option<&a206_core::BrokerPairing>,
    ) -> Result<Self, PushError> {
        Err(PushError::NotPaired)
    }

    /// One round of the push; the successful answer's body.
    #[cfg(unix)]
    async fn call(&mut self, call: a206_broker::GitCall) -> Result<Vec<u8>, PushError> {
        let reply = self
            .client
            .call(a206_broker::Request::GitReceivePack(call))
            .await
            .map_err(PushError::Broker)?;
        let a206_broker::Reply::Provider { status, body } = reply else {
            return Err(PushError::UnexpectedReply);
        };
        match status {
            200..=299 => Ok(body),
            401 | 403 => Err(PushError::Refused { status }),
            404 => Err(PushError::NoRepository),
            status => Err(PushError::Status { status }),
        }
    }
    #[cfg(not(unix))]
    async fn call(&mut self, _call: a206_broker::GitCall) -> Result<Vec<u8>, PushError> {
        Err(PushError::NotPaired)
    }
}

impl ToolHost for PushHost {
    fn list<'a>(&'a self, scope: &'a ToolScope) -> BoxFuture<'a, ToolListing> {
        Box::pin(async move {
            let configured = self
                .pushes
                .configured()
                .inspect_err(|error| {
                    crate::error::handle("git_push", error);
                })
                .unwrap_or(false);
            ToolListing {
                tools: if configured && scope.workspace.is_some() {
                    vec![Self::spec()]
                } else {
                    vec![]
                },
                unavailable: vec![],
            }
        })
    }
    fn call<'a>(
        &'a self,
        scope: &'a ToolScope,
        request: ToolRequest,
    ) -> BoxFuture<'a, Result<ToolOutput, ErrorReport>> {
        Box::pin(async move {
            self.run(scope, &request)
                .await
                .map(|value| ToolOutput {
                    text: value.to_string(),
                    is_error: false,
                })
                .map_err(fail)
        })
    }
}
