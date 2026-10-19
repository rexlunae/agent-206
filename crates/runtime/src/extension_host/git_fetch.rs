//! The repository side of the native `git_fetch` tool (see
//! [`crate::git_remote`]): which remote branch or tag is fetched from where,
//! what this repository tells the server it has, and storing what arrives
//! under the remote-tracking branch, or for a tag under the local tag. Like a push, the repository is opened as the
//! git host calls open it, nothing it configures runs, and where git would
//! run a program, rewrite the URL or map the branch differently, the fetch
//! is refused.
use super::{
    git::{check, git_failure, open, unsupported},
    git_branch::branch_name,
    git_commit::{Settings, has_hooks},
    git_push::{
        MAX_ANCESTRY, Named, RefKind, ancestor, direct, head_branch, host, peeled, remote_name,
        remote_url, settings, tag_chain,
    },
    git_tag::tag_name,
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
/// Most branches a tag fetch takes as complete local history.
const MAX_TAG_TIPS: usize = 1_000;

/// A remote branch or tag to fetch and where it is kept.
#[derive(Debug, Clone)]
pub(crate) struct FetchTarget {
    pub kind: RefKind,
    /// The branch's or tag's name on the remote, such as `main` or `v1.2`.
    pub name: String,
    pub remote: String,
    /// The repository URL the remote fetches from.
    pub url: url::Url,
    pub hash: gix::hash::Kind,
    /// Whether the remote's fetch refspec allows a forced update (`+`);
    /// never for a tag.
    pub force: bool,
    /// The remote-tracking branch's commit (for a tag, the local tag's
    /// object) before the fetch, if it exists.
    pub previous: Option<ObjectId>,
    /// Local commits whose history is complete here: the remote-tracking
    /// branch, the branch of the same name and HEAD (for a tag, HEAD, every
    /// local and remote-tracking branch and the existing tag's commit).
    pub tips: Vec<ObjectId>,
    /// Commits to tell the server about, newest first.
    pub haves: Vec<ObjectId>,
}
impl FetchTarget {
    /// The reference the branch or tag is on the server.
    pub fn reference(&self) -> String {
        match self.kind {
            RefKind::Branch => format!("refs/heads/{}", self.name),
            RefKind::Tag => format!("refs/tags/{}", self.name),
        }
    }
    /// Where it is kept here: the remote-tracking branch, such as
    /// `refs/remotes/origin/main`, or the local tag of the same name, as
    /// `git fetch <remote> tag <tag>` does.
    pub fn tracking(&self) -> String {
        match self.kind {
            RefKind::Branch => format!("refs/remotes/{}/{}", self.remote, self.name),
            RefKind::Tag => self.reference(),
        }
    }
    /// What `tracking` points at now: a branch's commit, or the object a
    /// tag names.
    fn current(&self, repo: &gix::Repository) -> Result<Option<ObjectId>, RemoteError> {
        match self.kind {
            RefKind::Branch => peeled(repo, &self.tracking()),
            RefKind::Tag => direct(repo, &self.tracking()),
        }
        .map_err(host)
    }
    /// The commit `tip` leads to, through a tag's tag objects; `None` when
    /// one of those objects is missing here.
    fn commit(
        &self,
        repo: &gix::Repository,
        tip: ObjectId,
    ) -> Result<Option<ObjectId>, RemoteError> {
        match self.kind {
            RefKind::Branch => Ok(Some(tip)),
            RefKind::Tag => Ok(tag_chain(repo, tip, &self.name)?.map(|(commit, _)| commit)),
        }
    }
}

/// Resolve what to fetch (a branch defaults to what the current branch
/// merges from, else its own name) and `remote` (default: the current
/// branch's remote, else `origin`) in the project at `root`.
pub(crate) fn target(
    root: PathBuf,
    fetched: Named<'_>,
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
    if matches!(fetched, Named::Branch(_)) {
        // The remote-tracking branch's reflog records who fetched.
        settings.identity().map_err(host)?;
    }
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
    let branch = match (fetched, &current) {
        (Named::Tag(tag), _) => return tag_target(&repo, &settings, tag, remote, interrupt),
        (Named::Branch(Some(name)), _) => name.to_owned(),
        (Named::Branch(None), Some(local)) => settings
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
        (Named::Branch(None), None) => {
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
    let url = fetch_url(&settings, &remote)?;
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
        kind: RefKind::Branch,
        name: branch,
        remote,
        url,
        hash: repo.object_hash(),
        force,
        previous,
        tips,
        haves,
    })
}

/// [`target`] for a tag: kept under the local tag of the same name, which
/// must not already name another object. No refspec applies, as for `git
/// fetch <remote> tag <tag>`, and no reflog records who fetched.
fn tag_target(
    repo: &gix::Repository,
    settings: &Settings,
    tag: &str,
    remote: String,
    interrupt: &AtomicBool,
) -> Result<FetchTarget, RemoteError> {
    tag_name(tag).ok_or(RemoteError::TagName)?;
    let url = fetch_url(settings, &remote)?;
    let mut target = FetchTarget {
        kind: RefKind::Tag,
        name: tag.to_owned(),
        remote,
        url,
        hash: repo.object_hash(),
        force: false,
        previous: None,
        tips: vec![],
        haves: vec![],
    };
    target.previous = target.current(repo)?;
    // A tag may name any history, so every local and remote-tracking
    // branch bounds the completeness walk, as does the commit an existing
    // tag of that name leads to.
    // An unborn HEAD has no commit; a HEAD that cannot be read fails.
    let head = repo.head().map_err(|e| host(git_failure("head", &e)))?;
    target.tips.extend(head.id().map(|id| id.detach()));
    let references = repo
        .references()
        .map_err(|e| host(git_failure("branches", &e)))?;
    let branches = references
        .local_branches()
        .map_err(|e| host(git_failure("branches", &e)))?
        .chain(
            references
                .remote_branches()
                .map_err(|e| host(git_failure("branches", &e)))?,
        );
    for reference in branches.take(MAX_TAG_TIPS) {
        check(interrupt).map_err(host)?;
        let mut reference = reference.map_err(|e| host(git_failure("branches", &e)))?;
        // A broken branch bounds nothing.
        if let Ok(id) = reference.peel_to_id() {
            target.tips.push(id.detach());
        }
    }
    if let Some(previous) = target.previous {
        target.tips.extend(target.commit(repo, previous)?);
    }
    target.tips.sort();
    target.tips.dedup();
    target.haves = haves(repo, &target.tips, interrupt)?;
    Ok(target)
}

/// The HTTPS URL `remote` fetches from; one that a setting would rewrite is
/// refused.
fn fetch_url(settings: &Settings, remote: &str) -> Result<url::Url, RemoteError> {
    let configured = settings
        .string(&format!("remote.{remote}.url"))
        .ok_or_else(|| RemoteError::NoRemote(remote.to_owned()))?;
    let configured = std::str::from_utf8(&configured).map_err(|_| RemoteError::RemoteUrl)?;
    if settings.rewrites(configured, false) {
        return Err(RemoteError::Rewritten);
    }
    remote_url(configured)
}

/// The name and URL of `remote` (default: the current branch's remote,
/// else `origin`) in the project at `root`, and the current branch, for the
/// pull request tools.
pub(crate) fn remote_repository(
    root: PathBuf,
    remote: Option<&str>,
    interrupt: &AtomicBool,
) -> Result<(String, url::Url, Option<String>), RemoteError> {
    let repo = open(root, interrupt).map_err(host)?;
    let settings = settings(&repo).map_err(host)?;
    let current = head_branch(&repo)?;
    let remote = remote_name(&settings, current.as_deref().unwrap_or_default(), remote)?;
    let url = fetch_url(&settings, &remote)?;
    Ok((remote, url, current))
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
    let Some(commit) = target.commit(&repo, tip)? else {
        return Ok(true);
    };
    match a206_git_push::check_complete(
        &repo,
        commit,
        target.tips.iter().copied(),
        limits,
        interrupt,
    ) {
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
    /// The commit fetched: the tip, or the commit a tag leads to.
    pub commit: ObjectId,
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
        Ok((forced, commit)) => {
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
            Ok(Fetched {
                objects,
                forced,
                commit,
            })
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

/// Check `tip` is complete and move the remote-tracking branch (or create
/// the tag) to it, only from the value it had when the fetch began. Whether
/// the move was a forced update, and the commit fetched.
fn record(
    root: PathBuf,
    target: &FetchTarget,
    tip: ObjectId,
    limits: Limits,
    interrupt: &AtomicBool,
) -> Result<(bool, ObjectId), RemoteError> {
    // Opened again so the new pack is seen.
    let repo = open(root, interrupt).map_err(host)?;
    let commit = target
        .commit(&repo, tip)?
        .ok_or(RemoteError::Pack(PackError::Incomplete))?;
    a206_git_push::check_complete(
        &repo,
        commit,
        target.tips.iter().copied(),
        limits,
        interrupt,
    )?;
    let forced = match target.previous {
        None => false,
        // Nothing to write, as long as nothing else moved it meanwhile.
        Some(previous) if previous == tip => {
            return if target.current(&repo)? == Some(previous) {
                Ok((false, commit))
            } else {
                Err(RemoteError::TrackingMoved(target.tracking()))
            };
        }
        // A tag is never replaced.
        Some(_) if target.kind == RefKind::Tag => {
            return Err(RemoteError::LocalTagExists(target.name.clone()));
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
    // Required for a branch, whose reflog records who fetched (checked when
    // the fetch began); a tag uses it when set, as git does.
    let signature = match settings.identity() {
        Ok((name, email)) => Some(gix::actor::Signature {
            name,
            email,
            time: gix::date::Time::now_local_or_utc(),
        }),
        Err(_) if target.kind == RefKind::Tag => None,
        Err(e) => return Err(host(e)),
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
                } else if target.kind == RefKind::Tag {
                    "fetch: storing tag".into()
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
    let committer = signature.as_ref().map(|s| s.to_ref(&mut time));
    match repo.edit_references_as([edit], committer) {
        Ok(_) => Ok((forced, commit)),
        // Another fetch, push or tag moved it since this one began.
        Err(_) if target.current(&repo)? != target.previous => {
            Err(RemoteError::TrackingMoved(tracking))
        }
        Err(e) => Err(host(git_failure("fetch", &e))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nonce() -> String {
        uuid::Uuid::new_v4().simple().to_string()
    }
    /// A commit on `reference` with a new file, on top of `parent`.
    fn commit(repo: &gix::Repository, reference: &str, parent: Option<ObjectId>) -> ObjectId {
        let base = match parent {
            Some(parent) => repo
                .find_commit(parent)
                .unwrap()
                .tree_id()
                .unwrap()
                .detach(),
            None => ObjectId::empty_tree(repo.object_hash()),
        };
        let blob = repo.write_blob(nonce().as_bytes()).unwrap().detach();
        let mut tree = repo.edit_tree(base).unwrap();
        tree.upsert(nonce(), gix::objs::tree::EntryKind::Blob, blob)
            .unwrap();
        let tree = tree.write().unwrap().detach();
        let signature = gix::actor::Signature {
            name: "n".into(),
            email: "n@example.invalid".into(),
            time: gix::date::Time::new(2_000_000, 0),
        };
        let (mut a, mut b) = Default::default();
        repo.commit_as(
            signature.to_ref(&mut a),
            signature.to_ref(&mut b),
            reference,
            nonce(),
            tree,
            parent,
        )
        .unwrap()
        .detach()
    }

    #[test]
    fn a_tag_on_a_long_side_branch_is_complete_without_walking_it() {
        let dir = tempfile::tempdir().unwrap();
        let repo = gix::init(dir.path()).unwrap();
        let root = commit(&repo, "HEAD", None);
        // A side branch longer than the walk may go, which HEAD lacks.
        let side = format!("refs/heads/s-{}", nonce());
        let mut tip = root;
        for _ in 0..4 {
            tip = commit(&repo, &side, Some(tip));
        }
        let config = dir.path().join(".git/config");
        let mut text = std::fs::read_to_string(&config).unwrap();
        text.push_str("[remote \"origin\"]\n\turl = https://example.invalid/a.git\n");
        std::fs::write(&config, text).unwrap();
        let tag = nonce();
        let never = AtomicBool::new(false);
        let target = target(dir.path().into(), Named::Tag(&tag), None, &never).unwrap();
        let short = Limits {
            commits: 2,
            objects: 1_000,
            bytes: 1 << 20,
            object: 1 << 20,
        };
        // The side branch bounds the walk, so a tag of its tip needs no pack.
        assert!(!needs_pack(dir.path().into(), &target, tip, short, &never).unwrap());
        // A commit beyond every branch is walked, and the limit applies.
        let beyond = commit(&repo, &format!("refs/scratch/{}", nonce()), Some(tip));
        let beyond = commit(&repo, &format!("refs/scratch/{}", nonce()), Some(beyond));
        let beyond = commit(&repo, &format!("refs/scratch/{}", nonce()), Some(beyond));
        assert!(matches!(
            needs_pack(dir.path().into(), &target, beyond, short, &never),
            Err(RemoteError::Pack(PackError::TooManyCommits(2)))
        ));
    }
}
