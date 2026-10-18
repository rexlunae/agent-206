//! The repository side of the native `git_fetch` tool (see
//! [`crate::git_remote`]): which remote branch is fetched from where, what
//! this repository tells the server it has, and storing what arrives under
//! the remote-tracking branch. Like a push, the repository is opened as the
//! git host calls open it, nothing it configures runs, and where git would
//! run a program, rewrite the URL or map the branch differently, the fetch
//! is refused.
use super::{
    git::{check, git_failure, open, unsupported},
    git_branch::branch_name,
    git_commit::has_hooks,
    git_push::{
        MAX_ANCESTRY, ancestor, head_branch, host, peeled, remote_name, remote_url, settings,
    },
};
use crate::git_remote::RemoteError;
use a206_git_push::{Limits, MAX_HAVES, PackError, Stored};
use gix::{
    ObjectId,
    refs::{
        Target,
        transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog},
    },
};
use std::{path::PathBuf, sync::atomic::AtomicBool};

/// Hooks git runs for a fetch.
const FETCH_HOOKS: [&str; 1] = ["reference-transaction"];

/// A remote branch to fetch and where it is kept.
#[derive(Debug, Clone)]
pub(crate) struct FetchTarget {
    /// The branch's name on the remote, such as `main`.
    pub branch: String,
    pub remote: String,
    /// The repository URL the remote fetches from.
    pub url: url::Url,
    pub hash: gix::hash::Kind,
    /// Whether the remote's fetch refspec allows a forced update (`+`).
    pub force: bool,
    /// The remote-tracking branch's commit before the fetch, if it exists.
    pub previous: Option<ObjectId>,
    /// Local commits whose history is complete here: the remote-tracking
    /// branch, the branch of the same name and HEAD.
    pub tips: Vec<ObjectId>,
    /// Commits to tell the server about, newest first.
    pub haves: Vec<ObjectId>,
}
impl FetchTarget {
    /// The reference the branch is on the server.
    pub fn reference(&self) -> String {
        format!("refs/heads/{}", self.branch)
    }
    /// The remote-tracking branch, such as `refs/remotes/origin/main`.
    pub fn tracking(&self) -> String {
        format!("refs/remotes/{}/{}", self.remote, self.branch)
    }
}

/// Resolve `branch` (default: what the current branch merges from, else
/// its own name) and `remote` (default: the current branch's remote, else
/// `origin`) in the project at `root`.
pub(crate) fn target(
    root: PathBuf,
    branch: Option<&str>,
    remote: Option<&str>,
    interrupt: &AtomicBool,
) -> Result<FetchTarget, RemoteError> {
    let repo = open(root, interrupt).map_err(host)?;
    let settings = settings(&repo).map_err(host)?;
    if has_hooks(&repo, &settings, &FETCH_HOOKS) {
        return Err(host(unsupported(
            "the repository has a reference-transaction hook, which git would run",
        )));
    }
    if repo.git_dir().join("shallow").exists() {
        return Err(RemoteError::Shallow);
    }
    // The remote-tracking branch's reflog records who fetched.
    settings.identity().map_err(host)?;
    let current = head_branch(&repo)?;
    let remote = remote_name(&settings, current.as_deref().unwrap_or_default(), remote)?;
    if settings
        .string("extensions.partialClone")
        .is_some_and(|value| !value.is_empty())
        || settings
            .string(&format!("remote.{remote}.promisor"))
            .is_some()
    {
        return Err(RemoteError::PartialClone);
    }
    let branch = match (branch, &current) {
        (Some(name), _) => name.to_owned(),
        (None, Some(local)) => settings
            .string(&format!("branch.{local}.merge"))
            .filter(|_| {
                settings
                    .string(&format!("branch.{local}.remote"))
                    .is_some_and(|configured| configured == remote.as_str())
            })
            .and_then(|merge| {
                std::str::from_utf8(&merge)
                    .ok()
                    .and_then(|merge| merge.strip_prefix("refs/heads/"))
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| local.clone()),
        (None, None) => {
            return Err(host(unsupported(
                "HEAD is not on a branch; name the branch to fetch",
            )));
        }
    };
    branch_name(&branch).ok_or(RemoteError::BranchName)?;
    // Only the mapping `git clone` sets up is followed, and only when it is
    // the remote's one mapping: git would also update what others map to.
    let mapping = format!("refs/heads/*:refs/remotes/{remote}/*");
    let refspecs = settings.strings(&format!("remote.{remote}.fetch"));
    let force = match refspecs.as_slice() {
        [spec] if *spec == format!("+{mapping}") => true,
        [spec] if *spec == mapping => false,
        _ => return Err(RemoteError::Refspec(remote)),
    };
    let configured = settings
        .string(&format!("remote.{remote}.url"))
        .ok_or_else(|| RemoteError::NoRemote(remote.clone()))?;
    let configured = std::str::from_utf8(&configured).map_err(|_| RemoteError::RemoteUrl)?;
    if settings.rewrites(configured, false) {
        return Err(RemoteError::Rewritten);
    }
    let url = remote_url(configured)?;
    let tracking = format!("refs/remotes/{remote}/{branch}");
    let previous = peeled(&repo, &tracking).map_err(host)?;
    let mut tips: Vec<ObjectId> = previous.into_iter().collect();
    tips.extend(peeled(&repo, &format!("refs/heads/{branch}")).map_err(host)?);
    // An unborn HEAD has no commit; a HEAD that cannot be read fails.
    let head = repo.head().map_err(|e| host(git_failure("head", &e)))?;
    tips.extend(head.id().map(|id| id.detach()));
    tips.sort();
    tips.dedup();
    let haves = haves(&repo, &tips, interrupt)?;
    Ok(FetchTarget {
        branch,
        remote,
        url,
        hash: repo.object_hash(),
        force,
        previous,
        tips,
        haves,
    })
}

/// Up to [`MAX_HAVES`] commits reachable from `tips`, newest first.
fn haves(
    repo: &gix::Repository,
    tips: &[ObjectId],
    interrupt: &AtomicBool,
) -> Result<Vec<ObjectId>, RemoteError> {
    if tips.is_empty() {
        return Ok(vec![]);
    }
    let walk = repo
        .rev_walk(tips.iter().copied())
        .sorting(gix::revision::walk::Sorting::ByCommitTime(
            Default::default(),
        ))
        .all()
        .map_err(|e| host(git_failure("log", &e)))?;
    let mut haves = vec![];
    for info in walk.take(MAX_HAVES) {
        check(interrupt).map_err(host)?;
        haves.push(info.map_err(|e| host(git_failure("log", &e)))?.id);
    }
    Ok(haves)
}

/// Whether the server's `tip` needs a pack: it does unless this repository
/// already has it with everything it reaches.
pub(crate) fn needs_pack(
    root: PathBuf,
    target: &FetchTarget,
    tip: ObjectId,
    limits: Limits,
    interrupt: &AtomicBool,
) -> Result<bool, RemoteError> {
    let repo = open(root, interrupt).map_err(host)?;
    match a206_git_push::check_complete(&repo, tip, target.tips.iter().copied(), limits, interrupt)
    {
        Ok(()) => Ok(false),
        Err(PackError::Incomplete) => Ok(true),
        Err(error) => Err(error.into()),
    }
}

/// What a fetch stored and recorded.
pub(crate) struct Fetched {
    /// Objects in the pack; 0 when none was needed.
    pub objects: usize,
    /// Whether the remote-tracking branch moved to a commit that does not
    /// contain its previous one.
    pub forced: bool,
}

/// Store `pack` (if any), check that `tip` is complete, and point the
/// remote-tracking branch at it. A pack that fails is removed again.
pub(crate) fn store(
    root: PathBuf,
    target: &FetchTarget,
    tip: ObjectId,
    pack: Option<&[u8]>,
    limits: Limits,
    interrupt: &AtomicBool,
) -> Result<Fetched, RemoteError> {
    let repo = open(root.clone(), interrupt).map_err(host)?;
    let stored = match pack {
        Some(pack) => Some(a206_git_push::store_pack(&repo, pack, limits, interrupt)?),
        None => None,
    };
    let objects = stored.as_ref().map_or(0, |stored| stored.objects);
    match record(root, target, tip, limits, interrupt) {
        Ok(forced) => {
            if let Some(stored) = stored {
                // The reference now keeps the objects; a `.keep` file left
                // behind only stops git from repacking them.
                if let Err((path, error)) = stored.release() {
                    tracing::warn!(
                        error = &error as &dyn std::error::Error,
                        path = %path.display(),
                        "could not remove a fetched pack's keep file"
                    );
                }
            }
            Ok(Fetched { objects, forced })
        }
        Err(error) => {
            if let Some(stored) = stored {
                discard(stored);
            }
            Err(error)
        }
    }
}

fn discard(stored: Stored) {
    if let Err((path, error)) = stored.discard() {
        // The fetch already fails with its own error; an unreferenced pack
        // is harmless and git collects it later.
        tracing::warn!(
            error = &error as &dyn std::error::Error,
            path = %path.display(),
            "could not remove a refused fetched pack"
        );
    }
}

/// Check `tip` is complete and move the remote-tracking branch to it, only
/// from the value it had when the fetch began. `true` when the move was a
/// forced update.
fn record(
    root: PathBuf,
    target: &FetchTarget,
    tip: ObjectId,
    limits: Limits,
    interrupt: &AtomicBool,
) -> Result<bool, RemoteError> {
    // Opened again so the new pack is seen.
    let repo = open(root, interrupt).map_err(host)?;
    a206_git_push::check_complete(&repo, tip, target.tips.iter().copied(), limits, interrupt)?;
    let forced = match target.previous {
        None => false,
        // Nothing to write, as long as nothing else moved it meanwhile.
        Some(previous) if previous == tip => {
            return if peeled(&repo, &target.tracking()).map_err(host)? == Some(previous) {
                Ok(false)
            } else {
                Err(RemoteError::TrackingMoved(target.tracking()))
            };
        }
        Some(previous) => {
            match ancestor(&repo, previous, tip, MAX_ANCESTRY, interrupt).map_err(host)? {
                Some(true) => false,
                Some(false) | None if target.force => true,
                Some(false) | None => return Err(RemoteError::Diverged(target.tracking())),
            }
        }
    };
    let settings = settings(&repo).map_err(host)?;
    let (name, email) = settings.identity().map_err(host)?;
    let signature = gix::actor::Signature {
        name,
        email,
        time: gix::date::Time::now_local_or_utc(),
    };
    let tracking = target.tracking();
    let full: gix::refs::FullName = tracking
        .as_str()
        .try_into()
        .map_err(|_| RemoteError::BranchName)?;
    let edit = RefEdit {
        change: Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: if forced {
                    "fetch: forced-update".into()
                } else {
                    "fetch: fast-forward".into()
                },
            },
            expected: match target.previous {
                None => PreviousValue::MustNotExist,
                Some(previous) => PreviousValue::MustExistAndMatch(Target::Object(previous)),
            },
            new: Target::Object(tip),
        },
        name: full,
        deref: false,
    };
    let mut time = Default::default();
    match repo.edit_references_as([edit], Some(signature.to_ref(&mut time))) {
        Ok(_) => Ok(forced),
        // Another fetch or push moved it since this one began.
        Err(_) if peeled(&repo, &tracking).map_err(host)? != target.previous => {
            Err(RemoteError::TrackingMoved(tracking))
        }
        Err(e) => Err(host(git_failure("fetch", &e))),
    }
}
