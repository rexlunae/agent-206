//! Pushing and fetching branches and tags of the project's repository for
//! the native run engine: a [`ToolHost`] whose `git_push` sends a local
//! branch or tag to the branch or tag of the same name on one of the
//! repository's HTTPS or ssh remotes, as `git push` does, and whose `git_fetch`
//! brings a remote branch into its remote-tracking branch, or a remote tag
//! into the local tag of the same name, as `git fetch` does.
//!
//! Both reach a forge with the user's token, so the tools are denied until
//! the user permits them on the conversation, need the conversation's
//! GitWrite grant, and each call still waits for the user's approval. The
//! forge's token or SSH key stays in the vault: the user names, per forge
//! origin, which vault credential git calls use, and the broker attaches
//! the token, or signs ssh's authentication with the key, only for the
//! origin it is bound to (see [`crate::git_ssh`]). Git itself never runs; only fast-forward updates
//! of one branch, or one tag the server lacks, are pushed, as a pack of whole objects, and a fetch's pack
//! must likewise fit one broker message.
//!
//! Cancelling a call is not always safe: once the pack is sent, the server
//! may update its branch even though the call reports nothing.
use crate::{
    extension_host::{
        Interrupt, git_fetch as fetching,
        git_push::{self as repository, Named, PushTarget, RefKind, RemoteUrl, Tracked},
    },
    gateway::Pushes,
    tools::{ToolHost, ToolListing, ToolOutput, ToolRequest, ToolScope, ToolSpec},
};
use a206_core::{Capability, ErrorKind, ErrorReport, ToolPermission};
use a206_extensions::abi::{HostErrorCode, HostFailure};
use a206_git_push::{Limits, PackError, Service, Update, WireError};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
};

/// Pushes a branch to a remote.
pub const PUSH: &str = "git_push";
/// Fetches a remote branch into its remote-tracking branch.
pub const FETCH: &str = "git_fetch";

/// The most one push sends or one fetch receives: what fits one broker
/// message, less room for the update command.
pub const LIMITS: Limits = Limits {
    commits: 10_000,
    objects: 100_000,
    bytes: a206_broker::MAX_PROVIDER_BODY - 4096,
    // Each object is read whole; one this large would have to shrink
    // sixteenfold to fit the pack.
    object: (a206_broker::MAX_PROVIDER_BODY - 4096) * 16,
};

/// Why a push or fetch was refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("`{0}` has the wrong type")]
    Type(&'static str),
    #[error("unknown argument `{0}`")]
    Unknown(String),
    #[error("unknown git remote tool `{0}`")]
    UnknownTool(String),
    #[error("the user did not approve this call")]
    NotApproved,
    #[error("the conversation has no project")]
    NoProject,
    #[error(transparent)]
    Repository(HostFailure),
    #[error("the branch name is not one git accepts")]
    BranchName,
    #[error("there is no local branch named {0}")]
    NoBranch(String),
    #[error("the tag name is not one git accepts")]
    TagName,
    #[error("there is no local tag named {0}, or its object is missing")]
    NoTag(String),
    #[error(
        "tag {0} does not name a commit, directly or through at most 10 tag objects; only tags of commits can be pushed"
    )]
    TagTarget(String),
    #[error("name either `branch` or `tag`, not both")]
    BranchAndTag,
    #[error(
        "the server already has tag {0}, at a different object; tags are never replaced, so delete or rename one of them"
    )]
    TagExists(String),
    #[error(
        "a local tag {0} already exists at a different object than the remote's; tags are never replaced, so delete or rename the local one first"
    )]
    LocalTagExists(String),
    #[error("the remote has no tag named {tag}")]
    NoRemoteTag { tag: String },
    #[error("remote names may hold only letters, digits, `.`, `_` and `-`")]
    RemoteName,
    #[error("the repository has no remote named {0}")]
    NoRemote(String),
    #[error(
        "only HTTPS and ssh remotes can be pushed to or fetched from; the remote's URL is neither an HTTPS repository URL without a query or fragment nor an ssh one"
    )]
    RemoteUrl,
    #[error(
        "the remote's ssh URL is not one this tool follows: write it as ssh://[user@]host[:port]/path or [user@]host:path, with a plain host name and a path without `%` or control characters"
    )]
    SshUrl,
    #[error(
        "the ssh user name may hold only letters, digits, `.`, `_` and `-`, at most 64 of them, and may not start with `-`"
    )]
    SshUser,
    #[error("the path {} cannot be passed to ssh", .0.display())]
    SshPath(PathBuf),
    #[error("ssh remotes are reached only on Unix systems")]
    SshUnsupported,
    #[error("ssh could not be started")]
    SshStart(#[source] std::io::Error),
    #[error("the ssh server sent more than {0} bytes, more than one call may receive")]
    SshTooLarge(usize),
    #[error("the ssh connection closed before the request was sent")]
    SshClosed,
    #[error(
        "the host key of {host} is not in ~/.ssh/known_hosts or the profile's known_hosts, or has changed; verify it and add it there"
    )]
    SshHostKey { host: String },
    #[error(
        "{host} refused the vault's ssh key; check that its public key is added to your account there"
    )]
    SshDenied { host: String },
    #[error("ssh failed{}: {message}", .code.map(|c| format!(" (exit {c})")).unwrap_or_default())]
    SshFailed { code: Option<i32>, message: String },
    #[error(
        "remote {0} is an ssh path under a home directory, which no forge serves; the pull request tools need a forge repository"
    )]
    ForgeSsh(String),
    #[error(
        "the remote's URL carries a username or password; remove it, the vault supplies the credential"
    )]
    RemoteCredentials,
    #[error(
        "the remote's URL is rewritten by a url.*.insteadOf setting, which this tool does not apply"
    )]
    Rewritten,
    #[error(
        "no git credential is set for {origin}; add one in Settings, Git credentials, and bind the vault credential to {origin}"
    )]
    NoCredential { origin: String },
    #[error("the vault broker could not be reached")]
    Vault(#[source] a206_broker::ClientError),
    #[error("the vault broker failed the call")]
    Broker(#[source] a206_broker::ClientError),
    #[error("the vault is not unlocked for the gateway; unlock it on a trusted client")]
    NotPaired,
    #[error(
        "the running vault broker is too old for this git call; restart it with `agent206 vault stop`"
    )]
    BrokerTooOld,
    #[error("the vault broker sent an unexpected reply")]
    UnexpectedReply,
    #[error(
        "the server refused the credential (HTTP {status}); check that the token is valid and may use this repository"
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
    #[error("the remote has no branch named {branch}")]
    NoRemoteBranch { branch: String },
    #[error(
        "remote {0} does not map its branches to remote-tracking branches as `git clone` does (refs/heads/*:refs/remotes/{0}/*), as its only fetch mapping, which is all this tool follows"
    )]
    Refspec(String),
    #[error("the repository is shallow, so its history cannot be completed by a fetch here")]
    Shallow,
    #[error("the repository is a partial clone, whose missing objects this tool cannot fetch")]
    PartialClone,
    #[error(
        "the remote's branch no longer contains {0}, and the remote's fetch refspec does not allow a forced update"
    )]
    Diverged(String),
    #[error("{0} changed while the fetch ran; try again")]
    TrackingMoved(String),
    #[error("the gateway's state could not be read")]
    Gateway(#[source] crate::gateway::GatewayError),
    #[error("the repository work stopped unexpectedly")]
    Task(#[source] tokio::task::JoinError),
}
impl crate::error::Classified for RemoteError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::Type(_)
            | Self::Unknown(_)
            | Self::BranchName
            | Self::TagName
            | Self::BranchAndTag
            | Self::TagTarget(_)
            | Self::RemoteName
            | Self::SshUrl
            | Self::SshUser
            | Self::SshPath(_) => ErrorKind::InvalidRequest,
            Self::UnknownTool(_)
            | Self::NoBranch(_)
            | Self::NoTag(_)
            | Self::NoRemote(_)
            | Self::NoRepository
            | Self::NoRemoteBranch { .. }
            | Self::NoRemoteTag { .. } => ErrorKind::NotFound,
            Self::NotApproved
            | Self::NoCredential { .. }
            | Self::Refused { .. }
            | Self::SshHostKey { .. }
            | Self::SshDenied { .. } => ErrorKind::PermissionDenied,
            Self::SshStart(_) | Self::SshUnsupported => ErrorKind::Unavailable,
            Self::SshTooLarge(_) | Self::SshClosed => ErrorKind::Protocol,
            Self::SshFailed { .. } => ErrorKind::Provider,
            Self::NoProject
            | Self::RemoteUrl
            | Self::ForgeSsh(_)
            | Self::RemoteCredentials
            | Self::Rewritten
            | Self::History
            | Self::Refspec(_)
            | Self::Shallow
            | Self::PartialClone => ErrorKind::InvalidRequest,
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
            // The server answered in git's protocol and refused.
            Self::Wire(WireError::Server(_)) => ErrorKind::Provider,
            Self::UnexpectedReply | Self::Wire(_) => ErrorKind::Protocol,
            Self::Status { .. } | Self::Unpack(_) | Self::Unreported(_) => ErrorKind::Provider,
            Self::Pack(PackError::Git(_) | PackError::Compress(_)) => ErrorKind::Storage,
            Self::Pack(PackError::Interrupted) => ErrorKind::Cancelled,
            // What a server sent is not a usable pack.
            Self::Pack(PackError::NotAPack | PackError::Unreadable(_) | PackError::Incomplete) => {
                ErrorKind::Protocol
            }
            Self::Pack(_) => ErrorKind::InvalidRequest,
            Self::Moved(_)
            | Self::NotFastForward(_)
            | Self::TagExists(_)
            | Self::LocalTagExists(_)
            | Self::Rejected { .. }
            | Self::Diverged(_)
            | Self::TrackingMoved(_) => ErrorKind::Conflict,
            Self::Tracking { .. } => ErrorKind::Storage,
            Self::Task(_) => ErrorKind::Internal,
        })
    }
}
fn fail(error: RemoteError) -> ErrorReport {
    crate::error::report(&error)
}

/// Offers `git_push` and `git_fetch` in conversations with a project, once
/// the user has set a git credential for at least one forge.
pub struct RemoteHost {
    pushes: Pushes,
}
impl RemoteHost {
    pub fn new(pushes: Pushes) -> Self {
        Self { pushes }
    }

    fn fetch_spec() -> ToolSpec {
        ToolSpec {
            name: FETCH.into(),
            description: "Fetch a branch from one of the project's HTTPS or ssh git remotes into its remote-tracking branch, like `git fetch <remote> <branch>` with the remote's usual refs/heads/*:refs/remotes/<remote>/* mapping, or a tag into the local tag of the same name, like `git fetch <remote> tag <tag>`. Only the remote-tracking branch (such as origin/main) or the new tag and the object store change; local branches and the working tree are left alone, and an existing local tag at another object is never replaced. The user approves each call. Refused when the repository has a reference-transaction hook, is shallow or a partial clone, or the remote's URL is neither HTTPS nor ssh.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "branch": {"type": "string", "description": "The branch's name on the remote, such as main. Defaults to the branch the current branch merges from, else the current branch's own name, unless `tag` is given."},
                    "tag": {"type": "string", "description": "A tag to fetch instead of a branch, such as v1.2, with the commits it needs."},
                    "remote": {"type": "string", "description": "The remote to fetch from. Defaults to the current branch's configured remote, else origin."}
                },
                "additionalProperties": false
            }),
            capabilities: [Capability::GitWrite].into(),
            permission: ToolPermission::Ask,
            whole_output: false,
        }
    }

    fn push_spec() -> ToolSpec {
        ToolSpec {
            name: PUSH.into(),
            description: "Push a local branch of the project's git repository to the branch of the same name on one of its HTTPS or ssh remotes, like `git push <remote> <branch>`, or a local tag to the tag of the same name, like `git push <remote> tag <tag>`. Only a fast-forward update of a branch is sent: if the remote branch has commits this branch lacks, the push is refused. A tag is sent only if the remote lacks it; one it has at a different object is never replaced. The user approves each call. Refused when the repository has a pre-push hook or the remote's URL is neither HTTPS nor ssh.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "branch": {"type": "string", "description": "The local branch to push, such as feature/login. Defaults to the current branch unless `tag` is given."},
                    "tag": {"type": "string", "description": "A local tag to push instead of a branch, such as v1.2. It is sent with the commits it needs."},
                    "remote": {"type": "string", "description": "The remote to push to. Defaults to the branch's configured remote (for a tag, the current branch's), else origin."}
                },
                "additionalProperties": false
            }),
            capabilities: [Capability::GitWrite].into(),
            permission: ToolPermission::Ask,
            whole_output: false,
        }
    }

    async fn run(&self, scope: &ToolScope, request: &ToolRequest) -> Result<Value, RemoteError> {
        let fetch = match request.name.as_str() {
            PUSH => false,
            FETCH => true,
            _ => return Err(RemoteError::UnknownTool(request.name.clone())),
        };
        // The gateway asks the user first; never act on a call it did not.
        if !request.approved {
            return Err(RemoteError::NotApproved);
        }
        let args = &request.arguments;
        if let Some(key) = args
            .keys()
            .find(|key| !matches!(key.as_str(), "branch" | "tag" | "remote"))
        {
            return Err(RemoteError::Unknown(key.clone()));
        }
        let branch = text(args, "branch")?;
        let tag = text(args, "tag")?;
        let remote = text(args, "remote")?;
        let root = scope.workspace.clone().ok_or(RemoteError::NoProject)?;
        // Dropping this future (a cancelled run) stops the repository work.
        let interrupt = Arc::new(AtomicBool::new(false));
        let _stop = Interrupt(Arc::clone(&interrupt));
        if branch.is_some() && tag.is_some() {
            return Err(RemoteError::BranchAndTag);
        }
        if fetch {
            self.fetch(root, branch, tag, remote, interrupt).await
        } else {
            self.push(root, branch, tag, remote, interrupt).await
        }
    }

    /// How a call of `service` reaches the repository at `url` with the
    /// credential the user set for its origin: through a broker that can
    /// make git calls of `version` (HTTPS), or ssh with the vault's key.
    async fn link(
        &self,
        url: &RemoteUrl,
        service: Service,
        version: u16,
    ) -> Result<Link, RemoteError> {
        let origin = url.origin();
        let credential = self
            .pushes
            .credential(&origin)
            .map_err(RemoteError::Gateway)?
            .ok_or_else(|| RemoteError::NoCredential {
                origin: origin.clone(),
            })?;
        let (broker_root, pairing) = self.pushes.broker().map_err(RemoteError::Gateway)?;
        match url {
            RemoteUrl::Https(url) => Ok(Link::Https {
                broker: Broker::connect(&broker_root, pairing.as_ref(), version).await?,
                call: GitCaller {
                    credential: credential.credential,
                    username: credential.username,
                    repository: url.to_string(),
                },
            }),
            #[cfg(unix)]
            RemoteUrl::Ssh(remote) => {
                let mut broker = Broker::connect(
                    &broker_root,
                    pairing.as_ref(),
                    a206_broker::SSH_AGENT_VERSION,
                )
                .await?;
                let key = broker
                    .ssh_agent(&credential.credential, &origin, None)
                    .await?
                    .0;
                // The URL's user, as git uses it; else the one the user set.
                let user = remote.user.as_deref().unwrap_or(&credential.username);
                let session = crate::git_ssh::SshSession::open(
                    remote,
                    user,
                    service,
                    &crate::connection::known_hosts(&broker_root),
                    key,
                    BrokerSigner {
                        broker,
                        credential: credential.credential,
                        destination: origin,
                    },
                )
                .await?;
                Ok(Link::Ssh(session))
            }
            #[cfg(not(unix))]
            RemoteUrl::Ssh(_) => {
                let _ = (broker_root, pairing, service);
                Err(RemoteError::SshUnsupported)
            }
        }
    }

    async fn fetch(
        &self,
        root: PathBuf,
        branch: Option<String>,
        tag: Option<String>,
        remote: Option<String>,
        interrupt: Arc<AtomicBool>,
    ) -> Result<Value, RemoteError> {
        let target = blocking({
            let (root, interrupt) = (root.clone(), Arc::clone(&interrupt));
            move || fetching::target(root, named(&branch, &tag), remote.as_deref(), &interrupt)
        })
        .await?;
        let service = Service::UploadPack;
        let mut link = self
            .link(&target.url, service, a206_broker::GIT_FETCH_VERSION)
            .await?;
        let advertised = link.advertisement(service, target.hash).await?;
        let tip = advertised
            .get(&target.reference())
            .ok_or_else(|| match target.kind {
                RefKind::Branch => RemoteError::NoRemoteBranch {
                    branch: target.name.clone(),
                },
                RefKind::Tag => RemoteError::NoRemoteTag {
                    tag: target.name.clone(),
                },
            })?;
        // Refused before anything is downloaded; checked again when stored.
        if target.kind == RefKind::Tag && target.previous.is_some_and(|local| local != tip) {
            return Err(RemoteError::LocalTagExists(target.name.clone()));
        }
        let needed = blocking({
            let (root, target, interrupt) = (root.clone(), target.clone(), Arc::clone(&interrupt));
            move || fetching::needs_pack(root, &target, tip, LIMITS, &interrupt)
        })
        .await?;
        let pack = if needed {
            let request = a206_git_push::fetch_request(&advertised, tip, &target.haves)?;
            let answer = link.send(service, request).await?;
            let side_band = advertised.capabilities.iter().any(|c| c == "side-band-64k");
            Some(a206_git_push::parse_fetch(&answer, side_band, target.hash)?)
        } else {
            link.close().await?;
            None
        };
        let fetched = blocking({
            let target = target.clone();
            move || fetching::store(root, &target, tip, pack.as_deref(), LIMITS, &interrupt)
        })
        .await?;
        tracing::info!(
            commit = %fetched.commit,
            reference = %target.tracking(),
            objects = fetched.objects,
            "git_fetch updated a reference"
        );
        let up_to_date = target.previous == Some(tip);
        Ok(match target.kind {
            RefKind::Branch => json!({
                "branch": target.name,
                "remote": target.remote,
                "tracking_branch": format!("{}/{}", target.remote, target.name),
                "from": target.previous.map(|id| id.to_string()),
                "commit": tip.to_string(),
                "objects": fetched.objects,
                "up_to_date": up_to_date,
                "forced_update": fetched.forced,
            }),
            RefKind::Tag => json!({
                "tag": target.name,
                "remote": target.remote,
                "annotated": tip != fetched.commit,
                "commit": fetched.commit.to_string(),
                "objects": fetched.objects,
                "up_to_date": up_to_date,
            }),
        })
    }

    async fn push(
        &self,
        root: PathBuf,
        branch: Option<String>,
        tag: Option<String>,
        remote: Option<String>,
        interrupt: Arc<AtomicBool>,
    ) -> Result<Value, RemoteError> {
        let target = blocking({
            let (root, interrupt) = (root.clone(), Arc::clone(&interrupt));
            move || repository::target(root, named(&branch, &tag), remote.as_deref(), &interrupt)
        })
        .await?;
        let service = Service::ReceivePack;
        let mut link = self
            .link(&target.url, service, a206_broker::GIT_VERSION)
            .await?;
        let advertised = link.advertisement(service, target.hash).await?;
        let packed = blocking({
            let (root, target) = (root.clone(), target.clone());
            move || {
                repository::pack(root, &target, &advertised, LIMITS, &interrupt)
                    .map(|packed| (packed, advertised))
            }
        })
        .await?;
        let (Some(packed), advertised) = packed else {
            link.close().await?;
            let mut result = pushed(&target);
            result.insert("up_to_date".into(), true.into());
            return Ok(result.into());
        };
        let update = Update {
            old: packed.old,
            new: target.tip,
            name: target.reference(),
        };
        let body = update.request(&advertised, &packed.pack)?;
        let answer = link.send(service, body).await?;
        let report = a206_git_push::parse_report(&answer, &update.name)?;
        report.unpack.map_err(RemoteError::Unpack)?;
        match report.reference {
            Some(Ok(())) => {}
            Some(Err(reason)) => {
                return Err(RemoteError::Rejected {
                    branch: target.label(),
                    reason,
                });
            }
            None => return Err(RemoteError::Unreported(target.label())),
        }
        tracing::info!(
            commit = %target.commit,
            reference = %target.reference(),
            commits = packed.commits,
            objects = packed.objects,
            "git_push updated a remote reference"
        );
        let mut result = pushed(&target);
        result.insert(
            "from".into(),
            (!packed.old.is_null())
                .then(|| packed.old.to_string())
                .into(),
        );
        result.insert("commits".into(), packed.commits.into());
        result.insert("objects".into(), packed.objects.into());
        result.insert("bytes".into(), packed.pack.len().into());
        // Git keeps no remote-tracking reference for tags.
        if target.kind == RefKind::Branch {
            let tracking = record(root, &target).await?;
            result.insert(
                "tracking_branch".into(),
                match tracking {
                    Tracked::Updated => "updated",
                    Tracked::Kept => "left at a newer commit",
                }
                .into(),
            );
        }
        Ok(result.into())
    }
}

/// A tag when one is given, else the branch (or its default).
fn named<'a>(branch: &'a Option<String>, tag: &'a Option<String>) -> Named<'a> {
    match tag {
        Some(tag) => Named::Tag(tag),
        None => Named::Branch(branch.as_deref()),
    }
}

/// What a push result says about what was pushed.
fn pushed(target: &PushTarget) -> Map<String, Value> {
    let mut result = Map::new();
    match target.kind {
        RefKind::Branch => {
            result.insert("branch".into(), target.name.clone().into());
        }
        RefKind::Tag => {
            result.insert("tag".into(), target.name.clone().into());
            result.insert("annotated".into(), (!target.tags.is_empty()).into());
        }
    }
    result.insert("remote".into(), target.remote.clone().into());
    result.insert("commit".into(), target.commit.to_string().into());
    result
}

/// Update the remote-tracking branch; the push itself already happened.
async fn record(root: PathBuf, target: &PushTarget) -> Result<Tracked, RemoteError> {
    let recorded = {
        let target = target.clone();
        tokio::task::spawn_blocking(move || repository::record(root, &target))
            .await
            .map_err(RemoteError::Task)?
    };
    recorded.map_err(|cause| RemoteError::Tracking {
        branch: target.name.clone(),
        remote: target.remote.clone(),
        cause,
    })
}

pub(crate) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, RemoteError> + Send + 'static,
) -> Result<T, RemoteError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(RemoteError::Task)?
}

pub(crate) fn text(
    args: &Map<String, Value>,
    name: &'static str,
) -> Result<Option<String>, RemoteError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(RemoteError::Type(name)),
    }
}

/// The git call for one repository with the credential set for its origin.
struct GitCaller {
    credential: String,
    username: String,
    repository: String,
}
impl GitCaller {
    fn call(&self, request: Option<Vec<u8>>) -> a206_broker::GitCall {
        a206_broker::GitCall {
            credential: self.credential.clone(),
            username: self.username.clone(),
            repository: self.repository.clone(),
            request,
        }
    }
}

/// How one push or fetch talks to the server: each round a broker call
/// (git's stateless smart HTTP), or one ssh conversation.
enum Link {
    Https {
        broker: Broker,
        call: GitCaller,
    },
    #[cfg(unix)]
    Ssh(crate::git_ssh::SshSession),
}
impl Link {
    /// The references and capabilities the server advertises.
    async fn advertisement(
        &mut self,
        service: Service,
        hash: gix::hash::Kind,
    ) -> Result<a206_git_push::Advertisement, RemoteError> {
        Ok(match self {
            Self::Https { broker, call } => {
                let body = broker.call(service, call.call(None)).await?;
                a206_git_push::parse_advertisement(&body, service, hash)?
            }
            #[cfg(unix)]
            Self::Ssh(session) => {
                let body = session.advertisement(LIMITS.bytes).await?;
                a206_git_push::parse_stream_advertisement(&body, hash)?
            }
        })
    }
    /// Send the service `request`; its answer.
    async fn send(self, service: Service, request: Vec<u8>) -> Result<Vec<u8>, RemoteError> {
        match self {
            Self::Https { mut broker, call } => {
                broker.call(service, call.call(Some(request))).await
            }
            #[cfg(unix)]
            Self::Ssh(session) => session.finish(Some(&request), MAX_ANSWER).await,
        }
    }
    /// End the conversation without a request.
    async fn close(self) -> Result<(), RemoteError> {
        match self {
            Self::Https { .. } => Ok(()),
            #[cfg(unix)]
            Self::Ssh(session) => session.finish(None, MAX_ANSWER).await.map(drop),
        }
    }
}
/// The most a service's answer over ssh may hold: what a broker relays.
#[cfg(unix)]
const MAX_ANSWER: usize = a206_broker::MAX_PROVIDER_BODY;

/// Signs an ssh client's authentication with the vault's key, through
/// the broker, for the one server the key is bound to.
#[cfg(unix)]
struct BrokerSigner {
    broker: Broker,
    credential: String,
    destination: String,
}
#[cfg(unix)]
impl crate::ssh_agent::Signer for BrokerSigner {
    type Error = RemoteError;
    fn sign(&mut self, data: Vec<u8>) -> BoxFuture<'_, Result<Vec<u8>, RemoteError>> {
        Box::pin(async move {
            self.broker
                .ssh_agent(&self.credential, &self.destination, Some(data))
                .await?
                .1
                .ok_or(RemoteError::UnexpectedReply)
        })
    }
}

/// A paired gateway connection to the vault broker that can make git and
/// forge API calls.
pub(crate) struct Broker {
    #[cfg(unix)]
    client: a206_broker::Client,
}
impl Broker {
    #[cfg(unix)]
    pub(crate) async fn connect(
        root: &std::path::Path,
        pairing: Option<&a206_core::BrokerPairing>,
        version: u16,
    ) -> Result<Self, RemoteError> {
        use a206_broker::{Request, Role, WireSecret};
        let pairing = pairing.ok_or(RemoteError::NotPaired)?;
        let mut client = a206_broker::Client::connect(root, Role::Gateway)
            .await
            .map_err(RemoteError::Vault)?;
        if client.version() < version {
            return Err(RemoteError::BrokerTooOld);
        }
        client
            .call(Request::GatewayAuth {
                token: WireSecret::new(pairing.expose().to_owned()),
            })
            .await
            .map_err(RemoteError::Broker)?;
        Ok(Self { client })
    }
    #[cfg(not(unix))]
    pub(crate) async fn connect(
        _root: &std::path::Path,
        _pairing: Option<&a206_core::BrokerPairing>,
        _version: u16,
    ) -> Result<Self, RemoteError> {
        Err(RemoteError::NotPaired)
    }

    /// One round of a push or fetch; the successful answer's body.
    #[cfg(unix)]
    async fn call(
        &mut self,
        service: Service,
        call: a206_broker::GitCall,
    ) -> Result<Vec<u8>, RemoteError> {
        let request = match service {
            Service::ReceivePack => a206_broker::Request::GitReceivePack(call),
            Service::UploadPack => a206_broker::Request::GitUploadPack(call),
        };
        let reply = self
            .client
            .call(request)
            .await
            .map_err(RemoteError::Broker)?;
        let a206_broker::Reply::Provider { status, body } = reply else {
            return Err(RemoteError::UnexpectedReply);
        };
        match status {
            200..=299 => Ok(body),
            401 | 403 => Err(RemoteError::Refused { status }),
            404 => Err(RemoteError::NoRepository),
            status => Err(RemoteError::Status { status }),
        }
    }
    #[cfg(not(unix))]
    async fn call(
        &mut self,
        _service: Service,
        _call: a206_broker::GitCall,
    ) -> Result<Vec<u8>, RemoteError> {
        Err(RemoteError::NotPaired)
    }

    /// The public key of the SSH key `credential`, bound to `destination`,
    /// and with `sign`, its signature of that authentication request.
    #[cfg(unix)]
    async fn ssh_agent(
        &mut self,
        credential: &str,
        destination: &str,
        sign: Option<Vec<u8>>,
    ) -> Result<(Vec<u8>, Option<Vec<u8>>), RemoteError> {
        let reply = self
            .client
            .call(a206_broker::Request::SshAgent(a206_broker::SshAgentCall {
                credential: credential.to_owned(),
                destination: destination.to_owned(),
                sign,
            }))
            .await
            .map_err(RemoteError::Broker)?;
        let a206_broker::Reply::SshAgent {
            public_key,
            signature,
        } = reply
        else {
            return Err(RemoteError::UnexpectedReply);
        };
        Ok((public_key, signature))
    }

    /// One forge API request; the status and the body of a success (the
    /// broker never relays the body of a failure). A PUT needs a broker
    /// that sends one.
    #[cfg(unix)]
    pub(crate) async fn provider(
        &mut self,
        call: a206_broker::ProviderCall,
    ) -> Result<(u16, Vec<u8>), RemoteError> {
        if matches!(call.method, a206_broker::HttpMethod::Put)
            && self.client.version() < a206_broker::PUT_VERSION
        {
            return Err(RemoteError::BrokerTooOld);
        }
        let reply = self
            .client
            .call(a206_broker::Request::Provider(call))
            .await
            .map_err(RemoteError::Broker)?;
        let a206_broker::Reply::Provider { status, body } = reply else {
            return Err(RemoteError::UnexpectedReply);
        };
        Ok((status, body))
    }
    #[cfg(not(unix))]
    pub(crate) async fn provider(
        &mut self,
        _call: a206_broker::ProviderCall,
    ) -> Result<(u16, Vec<u8>), RemoteError> {
        Err(RemoteError::NotPaired)
    }
}

impl ToolHost for RemoteHost {
    fn list<'a>(&'a self, scope: &'a ToolScope) -> BoxFuture<'a, ToolListing> {
        Box::pin(async move {
            let configured = self
                .pushes
                .configured()
                .inspect_err(|error| {
                    crate::error::handle("git_remote", error);
                })
                .unwrap_or(false);
            ToolListing {
                tools: if configured && scope.workspace.is_some() {
                    vec![Self::push_spec(), Self::fetch_spec()]
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
