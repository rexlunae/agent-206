//! Tags: listing them, and creating one at a commit, like
//! `git tag <name> [<commit>]` or, with a message, `git tag -a`. Only the
//! new reference (and for an annotated tag, its tag object) is written; the
//! working tree, the index and `HEAD` are left as they are.
use super::{
    failure,
    git::{check, git_failure, open, unsupported},
    git_branch::{nests, start_commit},
    git_commit::{MAX_COMMIT_MESSAGE, Settings, UserGit, has_hooks},
};
use a206_extensions::abi::{GitTag, GitTags, HostErrorCode, HostFailure, HostReply};
use gix::{
    ObjectId,
    bstr::ByteSlice,
    object::Kind,
    refs::{
        FullName, Target,
        transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog},
    },
};
use std::{path::PathBuf, sync::atomic::AtomicBool};

/// Most tags one listing returns.
pub const MAX_TAGS: usize = 500;
/// Most folders a tag name nests in, as for branches.
pub const MAX_TAG_FOLDERS: usize = 20;
/// The hook git runs when a reference changes. Its presence refuses the call.
const REFERENCE_HOOKS: [&str; 1] = ["reference-transaction"];

pub(super) fn list(root: PathBuf, interrupt: &AtomicBool) -> Result<HostReply, HostFailure> {
    let repo = open(root, interrupt)?;
    let references = repo.references().map_err(|e| git_failure("tags", &e))?;
    let mut listing = GitTags::default();
    for reference in references.tags().map_err(|e| git_failure("tags", &e))? {
        check(interrupt)?;
        let mut reference = reference.map_err(|e| git_failure("tags", &e))?;
        if listing.tags.len() == MAX_TAGS {
            listing.truncated = true;
            break;
        }
        let name = reference.name().shorten().to_str_lossy().into_owned();
        // A tag naming another (symbolic) is followed to the object it
        // names. One whose object is missing, or whose chain is broken, is
        // listed without a commit, as git still lists its name.
        let annotated = match reference.follow_to_object() {
            Ok(id) => kind(&repo, id.detach())? == Some(Kind::Tag),
            Err(_) => false,
        };
        let commit = match reference.peel_to_id() {
            Ok(peeled) => {
                let peeled = peeled.detach();
                (kind(&repo, peeled)? == Some(Kind::Commit)).then(|| peeled.to_string())
            }
            Err(_) => None,
        };
        listing.tags.push(GitTag {
            name,
            commit,
            annotated,
        });
    }
    check(interrupt)?;
    listing.tags.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(HostReply::Tags(listing))
}

/// The kind of object `id`, if the repository has it.
fn kind(repo: &gix::Repository, id: ObjectId) -> Result<Option<Kind>, HostFailure> {
    repo.try_find_header(id)
        .map(|header| header.map(|header| header.kind()))
        .map_err(|e| git_failure("tags", &e))
}

pub(super) fn create(
    root: PathBuf,
    name: &str,
    target: Option<&str>,
    message: Option<&str>,
    user: impl FnOnce() -> Result<UserGit, HostFailure>,
    interrupt: &AtomicBool,
) -> Result<HostReply, HostFailure> {
    if name.matches('/').count() > MAX_TAG_FOLDERS {
        return Err(failure(
            HostErrorCode::Invalid,
            "a tag name may nest at most 20 folders deep",
        ));
    }
    let full = tag_name(name).ok_or_else(|| {
        failure(
            HostErrorCode::Invalid,
            "the tag name is not one git accepts",
        )
    })?;
    if let Some(message) = message
        && (message.trim().is_empty()
            || message.len() > MAX_COMMIT_MESSAGE
            || message.contains('\0'))
    {
        return Err(failure(
            HostErrorCode::Invalid,
            "the tag message must be 1 to 16 KiB of text",
        ));
    }
    let repo = open(root, interrupt)?;
    let user = user()?;
    let settings = Settings::resolve(&repo, &user)?;
    if has_hooks(&repo, &settings, &REFERENCE_HOOKS) {
        return Err(unsupported(
            "the repository has a reference-transaction hook, which extensions never run",
        ));
    }
    // With tag.gpgSign git makes every tag a signed tag object, even one
    // asked for without a message. (tag.forceSignAnnotated yields to an
    // explicit annotation, which a message is.)
    if settings.flag("tag.gpgSign").unwrap_or(false) {
        return Err(unsupported(
            "this repository signs tags (tag.gpgSign), which extensions cannot do",
        ));
    }
    // Only an annotated tag records its tagger; git needs no identity for
    // a plain one, whose reflog (where git keeps one) then uses gitoxide's
    // default.
    let identity = match message {
        Some(_) => Some(settings.identity()?),
        None => settings.identity().ok(),
    };
    let commit = match target {
        None => repo
            .head_id()
            .map_err(|_| {
                failure(
                    HostErrorCode::Invalid,
                    "HEAD has no commit yet; make one first or name a commit",
                )
            })?
            .detach(),
        Some(target) => start_commit(&repo, target, "commit")?,
    };
    if repo
        .try_find_reference(&full)
        .map_err(|e| git_failure("tags", &e))?
        .is_some()
    {
        return Err(failure(HostErrorCode::Conflict, "the tag already exists"));
    }
    if nests(&repo, "refs/tags/", name)? {
        return Err(failure(
            HostErrorCode::Conflict,
            "a tag named like a folder of this one, or inside it, exists",
        ));
    }
    // The last point at which the call can stop without creating the tag. A
    // tag object written before a failed reference is unreferenced, like
    // any loose object git would collect.
    check(interrupt)?;
    let signature = identity.map(|(name, email)| gix::actor::Signature {
        name,
        email,
        time: gix::date::Time::now_local_or_utc(),
    });
    let points_at = match message {
        None => commit,
        Some(message) => {
            // An annotated tag always has an identity, as resolved above.
            let tagger = signature.clone();
            let mut message = message.to_owned();
            if !message.ends_with('\n') {
                message.push('\n');
            }
            repo.write_object(gix::objs::Tag {
                target: commit,
                target_kind: Kind::Commit,
                name: name.into(),
                tagger,
                message: message.into(),
                signature: None,
            })
            .map_err(|e| {
                tracing::warn!(
                    error = &e as &dyn std::error::Error,
                    "extension tag object could not be written"
                );
                failure(HostErrorCode::Io, "the tag could not be written")
            })?
            .detach()
        }
    };
    let mut time = Default::default();
    let edit = RefEdit {
        change: Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: "tag: created".into(),
            },
            expected: PreviousValue::MustNotExist,
            new: Target::Object(points_at),
        },
        name: full.clone(),
        deref: false,
    };
    let committer = signature
        .as_ref()
        .map(|signature| signature.to_ref(&mut time));
    if let Err(error) = repo.edit_references_as([edit], committer) {
        // Another process may have created it in between; anything else is
        // a write failure.
        let exists = matches!(repo.try_find_reference(&full), Ok(Some(_)));
        tracing::warn!(
            error = &error as &dyn std::error::Error,
            exists,
            "extension tag creation failed"
        );
        return Err(if exists {
            failure(HostErrorCode::Conflict, "the tag already exists")
        } else {
            failure(HostErrorCode::Io, "the tag could not be written")
        });
    }
    tracing::info!(commit = %commit, annotated = message.is_some(), "extension created a git tag");
    Ok(HostReply::Tag(GitTag {
        name: name.into(),
        commit: Some(commit.to_string()),
        annotated: message.is_some(),
    }))
}

/// `refs/tags/<name>`, for a name `git tag` accepts.
fn tag_name(name: &str) -> Option<FullName> {
    if name.is_empty() || name.starts_with('-') || name == "HEAD" {
        return None;
    }
    FullName::try_from(format!("refs/tags/{name}")).ok()
}
