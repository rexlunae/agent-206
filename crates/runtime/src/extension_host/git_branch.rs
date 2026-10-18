//! Local branches: listing them, and creating one at a commit without
//! switching to it, like `git branch <name> [<start>]`. Only a new reference
//! and its reflog entry are written; the working tree, the index and `HEAD`
//! are left as they are.
use super::{
    failure,
    git::{check, git_failure, open, unsupported},
    git_commit::{Settings, UserGit, has_hooks},
};
use a206_extensions::abi::{GitBranch, GitBranches, HostErrorCode, HostFailure, HostReply};
use gix::{
    ObjectId,
    bstr::ByteSlice,
    refs::{
        FullName, Target,
        transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog},
    },
};
use std::{path::PathBuf, sync::atomic::AtomicBool};

/// Most branches one listing returns.
pub const MAX_BRANCHES: usize = 500;
/// Most folders a branch name nests in. Git has no such limit, but
/// gitoxide creates at most 25 missing directories for one reference.
pub const MAX_BRANCH_FOLDERS: usize = 20;
/// The hook git runs when a reference changes. Its presence refuses the call.
const REFERENCE_HOOKS: [&str; 1] = ["reference-transaction"];

pub(super) fn list(root: PathBuf, interrupt: &AtomicBool) -> Result<HostReply, HostFailure> {
    let repo = open(root, interrupt)?;
    let head = repo.head().map_err(|e| git_failure("head", &e))?;
    let current = head.referent_name().map(|name| name.as_bstr().to_owned());
    let references = repo.references().map_err(|e| git_failure("branches", &e))?;
    let mut listing = GitBranches::default();
    for reference in references
        .local_branches()
        .map_err(|e| git_failure("branches", &e))?
    {
        check(interrupt)?;
        let mut reference = reference.map_err(|e| git_failure("branches", &e))?;
        if listing.branches.len() == MAX_BRANCHES {
            listing.truncated = true;
            break;
        }
        let id = reference
            .peel_to_id()
            .map_err(|e| git_failure("branches", &e))?
            .detach();
        let name = reference.name();
        listing.branches.push(GitBranch {
            name: name.shorten().to_str_lossy().into_owned(),
            commit: id.to_string(),
            current: current
                .as_ref()
                .is_some_and(|c| c.as_bstr() == name.as_bstr()),
        });
    }
    check(interrupt)?;
    listing.branches.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(HostReply::Branches(listing))
}

pub(super) fn create(
    root: PathBuf,
    name: &str,
    start: Option<&str>,
    user: impl FnOnce() -> Result<UserGit, HostFailure>,
    interrupt: &AtomicBool,
) -> Result<HostReply, HostFailure> {
    if name.matches('/').count() > MAX_BRANCH_FOLDERS {
        return Err(failure(
            HostErrorCode::Invalid,
            "a branch name may nest at most 20 folders deep",
        ));
    }
    let full = branch_name(name).ok_or_else(|| {
        failure(
            HostErrorCode::Invalid,
            "the branch name is not one git accepts",
        )
    })?;
    let repo = open(root, interrupt)?;
    let user = user()?;
    let settings = Settings::resolve(&repo, &user)?;
    if has_hooks(&repo, &settings, &REFERENCE_HOOKS) {
        return Err(unsupported(
            "the repository has a reference-transaction hook, which extensions never run",
        ));
    }
    let (committer, email) = settings.identity()?;
    let head = repo.head().map_err(|e| git_failure("head", &e))?;
    // An unborn current branch has no reference yet, but creating it would
    // give `HEAD` a commit.
    if head.referent_name() == Some(full.as_ref()) {
        return Err(failure(
            HostErrorCode::Conflict,
            "the branch is the current branch",
        ));
    }
    let id = match start {
        None => repo
            .head_id()
            .map_err(|_| {
                failure(
                    HostErrorCode::Invalid,
                    "HEAD has no commit yet; make one first or name a start",
                )
            })?
            .detach(),
        Some(start) => start_commit(&repo, start)?,
    };
    if repo
        .try_find_reference(&full)
        .map_err(|e| git_failure("branches", &e))?
        .is_some()
    {
        return Err(failure(
            HostErrorCode::Conflict,
            "the branch already exists",
        ));
    }
    if nests(&repo, name)? {
        return Err(failure(
            HostErrorCode::Conflict,
            "a branch named like a folder of this one, or inside it, exists",
        ));
    }
    // The last point at which the call can stop without creating the branch.
    check(interrupt)?;
    let signature = gix::actor::Signature {
        name: committer,
        email,
        time: gix::date::Time::now_local_or_utc(),
    };
    let mut time = Default::default();
    let edit = RefEdit {
        change: Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: format!("branch: Created from {}", start.unwrap_or("HEAD")).into(),
            },
            expected: PreviousValue::MustNotExist,
            new: Target::Object(id),
        },
        name: full.clone(),
        deref: false,
    };
    if let Err(error) = repo.edit_references_as([edit], Some(signature.to_ref(&mut time))) {
        // Another process may have created it in between; anything else is
        // a write failure.
        let exists = matches!(repo.try_find_reference(&full), Ok(Some(_)));
        tracing::warn!(
            error = &error as &dyn std::error::Error,
            exists,
            "extension branch creation failed"
        );
        return Err(if exists {
            failure(HostErrorCode::Conflict, "the branch already exists")
        } else {
            failure(HostErrorCode::Io, "the branch could not be written")
        });
    }
    tracing::info!(commit = %id, "extension created a git branch");
    Ok(HostReply::Branch(GitBranch {
        name: name.into(),
        commit: id.to_string(),
        current: false,
    }))
}

/// Whether a branch is a parent folder of `name`, or `name` is a folder of
/// branches: git stores both as paths, so they cannot exist together.
fn nests(repo: &gix::Repository, name: &str) -> Result<bool, HostFailure> {
    for (end, _) in name.match_indices('/') {
        let parent = format!("refs/heads/{}", &name[..end]);
        if repo
            .try_find_reference(parent.as_str())
            .map_err(|e| git_failure("branches", &e))?
            .is_some()
        {
            return Ok(true);
        }
    }
    let references = repo.references().map_err(|e| git_failure("branches", &e))?;
    let below = format!("refs/heads/{name}/");
    let mut inside = references
        .prefixed(below.as_str())
        .map_err(|e| git_failure("branches", &e))?;
    inside
        .next()
        .transpose()
        .map(|reference| reference.is_some())
        .map_err(|e| git_failure("branches", &e))
}

/// `refs/heads/<name>`, for a name `git branch` accepts.
fn branch_name(name: &str) -> Option<FullName> {
    if name.is_empty() || name.starts_with('-') || name == "HEAD" {
        return None;
    }
    FullName::try_from(format!("refs/heads/{name}")).ok()
}

/// A local branch's commit, or a commit named by its full id. Revision
/// expressions are not evaluated.
fn start_commit(repo: &gix::Repository, start: &str) -> Result<ObjectId, HostFailure> {
    let not_found = || {
        failure(
            HostErrorCode::NotFound,
            "the start is neither a local branch nor a full commit id",
        )
    };
    if let Some(branch) = branch_name(start)
        && let Some(mut reference) = repo
            .try_find_reference(&branch)
            .map_err(|e| git_failure("branches", &e))?
    {
        return Ok(reference
            .peel_to_commit()
            .map_err(|e| git_failure("branches", &e))?
            .id);
    }
    let id = ObjectId::from_hex(start.as_bytes()).map_err(|_| not_found())?;
    match repo.try_find_object(id) {
        Ok(Some(object)) if object.kind == gix::object::Kind::Commit => Ok(id),
        Ok(_) => Err(not_found()),
        Err(e) => Err(git_failure("branches", &e)),
    }
}
