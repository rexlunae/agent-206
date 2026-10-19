//! The repository side of the native `git_push` tool (see
//! [`crate::git_remote`]): which branch or tag goes where, the objects it
//! sends and the remote-tracking branch a branch push updates. The project's repository is opened
//! as the git host calls open it, and nothing it configures runs; where git
//! would run a program or rewrite the remote's URL, the push is refused.
use super::{
    git::{check, git_failure, open, unsupported},
    git_branch::branch_name,
    git_commit::{Settings, UserGit, has_hooks, includes_files},
    git_tag::tag_name,
};
use crate::git_remote::RemoteError;
use a206_extensions::abi::{HostErrorCode, HostFailure};
use a206_git_push::{Advertisement, Limits, PackError};
use gix::{
    ObjectId,
    refs::{
        Target,
        transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog},
    },
};
use std::{path::PathBuf, sync::atomic::AtomicBool};

/// Hooks git runs for a push.
const PUSH_HOOKS: [&str; 2] = ["pre-push", "reference-transaction"];
/// Most commits a fast-forward check walks back from the pushed commit.
pub(super) const MAX_ANCESTRY: usize = 100_000;
/// How often a remote-tracking branch update is retried after another update
/// changed the branch first.
const TRACKING_ATTEMPTS: usize = 3;
/// Most tag objects followed from a pushed tag to its commit.
const MAX_TAG_CHAIN: usize = 10;
/// Most symbolic references followed to an object, as git's own limit.
const MAX_SYMBOLIC_DEPTH: usize = 5;

/// What a push sends: a branch or a tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PushKind {
    Branch,
    Tag,
}

/// A branch or tag to push and where to.
#[derive(Debug, Clone)]
pub(crate) struct PushTarget {
    pub kind: PushKind,
    /// The local branch or tag, such as `feature/login` or `v1.2`.
    pub name: String,
    /// The object it points at: a branch's commit, or a tag's tag object
    /// (or commit, for a plain tag).
    pub tip: ObjectId,
    /// The commit `tip` leads to.
    pub commit: ObjectId,
    /// The tag objects between `tip` and `commit`, which a tag push sends
    /// with the commits; empty for a branch or a plain tag.
    pub tags: Vec<ObjectId>,
    pub remote: String,
    /// The repository URL the remote pushes to.
    pub url: url::Url,
    pub hash: gix::hash::Kind,
}
impl PushTarget {
    /// The reference the push updates on the server, by the same name.
    pub fn reference(&self) -> String {
        match self.kind {
            PushKind::Branch => format!("refs/heads/{}", self.name),
            PushKind::Tag => format!("refs/tags/{}", self.name),
        }
    }
    /// How failures name what was pushed: a branch by its name, a tag as
    /// `tag <name>`.
    pub fn label(&self) -> String {
        match self.kind {
            PushKind::Branch => self.name.clone(),
            PushKind::Tag => format!("tag {}", self.name),
        }
    }
}

pub(super) fn host(error: HostFailure) -> RemoteError {
    RemoteError::Repository(error)
}

/// The settings a push follows: the repository's own, then the user's
/// (with the includes the user wrote). A push is refused when the
/// repository's configuration includes other files: the project chooses
/// them, they may lie outside it, and git would follow them.
pub(super) fn settings(repo: &gix::Repository) -> Result<Settings, HostFailure> {
    if includes_files(repo)? {
        return Err(unsupported(
            "the repository's git configuration includes other files, which this push does not read",
        ));
    }
    Settings::resolve(repo, &UserGit::load()?)
}

/// What to push: a branch, the current one when neither is named, or a tag.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Pushed<'a> {
    Branch(Option<&'a str>),
    Tag(&'a str),
}

/// Resolve what to push and `remote` (default: the branch's configured
/// remote, for a tag the current branch's, else `origin`) in the project at
/// `root`.
pub(crate) fn target(
    root: PathBuf,
    pushed: Pushed<'_>,
    remote: Option<&str>,
    interrupt: &AtomicBool,
) -> Result<PushTarget, RemoteError> {
    let repo = open(root, interrupt).map_err(host)?;
    let settings = settings(&repo).map_err(host)?;
    if has_hooks(&repo, &settings, &PUSH_HOOKS) {
        return Err(host(unsupported(
            "the repository has a pre-push or reference-transaction hook, which git would run",
        )));
    }
    let (kind, name, tip, commit, tags, remote) = match pushed {
        Pushed::Branch(branch) => {
            // The remote-tracking branch's reflog records who pushed.
            settings.identity().map_err(host)?;
            let branch = match branch {
                Some(name) => name.to_owned(),
                None => current_branch(&repo)?,
            };
            let full = branch_name(&branch).ok_or(RemoteError::BranchName)?;
            let tip = repo
                .try_find_reference(&full)
                .map_err(|e| host(git_failure("branches", &e)))?
                .ok_or_else(|| RemoteError::NoBranch(branch.clone()))?
                .peel_to_id()
                .map_err(|_| RemoteError::NoBranch(branch.clone()))?
                .detach();
            let remote = remote_name(&settings, &branch, remote)?;
            (PushKind::Branch, branch, tip, tip, vec![], remote)
        }
        Pushed::Tag(tag) => {
            let full = tag_name(tag).ok_or(RemoteError::TagName)?;
            let tip = direct(&repo, full.as_bstr().to_string().as_str())
                .map_err(host)?
                .ok_or_else(|| RemoteError::NoTag(tag.to_owned()))?;
            let (commit, tags) = tag_chain(&repo, tip, tag)?;
            let current = head_branch(&repo)?;
            let remote = remote_name(&settings, current.as_deref().unwrap_or_default(), remote)?;
            (PushKind::Tag, tag.to_owned(), tip, commit, tags, remote)
        }
    };
    let configured = settings
        .string(&format!("remote.{remote}.pushurl"))
        .or_else(|| settings.string(&format!("remote.{remote}.url")))
        .ok_or_else(|| RemoteError::NoRemote(remote.clone()))?;
    let configured = std::str::from_utf8(&configured).map_err(|_| RemoteError::RemoteUrl)?;
    if settings.rewrites(configured, true) {
        return Err(RemoteError::Rewritten);
    }
    let url = remote_url(configured)?;
    Ok(PushTarget {
        kind,
        name,
        tip,
        commit,
        tags,
        remote,
        url,
        hash: repo.object_hash(),
    })
}

/// `remote`, else `branch`'s configured remote, else `origin`; refused
/// unless it is a plain name.
pub(super) fn remote_name(
    settings: &Settings,
    branch: &str,
    remote: Option<&str>,
) -> Result<String, RemoteError> {
    let remote = match remote {
        Some(remote) => remote.to_owned(),
        None => settings
            .string(&format!("branch.{branch}.remote"))
            .map(|remote| remote.to_string())
            .unwrap_or_else(|| "origin".to_owned()),
    };
    if remote.is_empty()
        || remote.len() > 100
        || !remote
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(RemoteError::RemoteName);
    }
    Ok(remote)
}

/// The object `reference` names, following symbolic references but not
/// tag objects, or `None` if it or a reference it names does not exist, or
/// the chain is longer than git follows. A reference that cannot be read is
/// a failure.
fn direct(repo: &gix::Repository, reference: &str) -> Result<Option<ObjectId>, HostFailure> {
    // Names stay bytes: a symbolic target need not be UTF-8.
    let Ok(mut name) = gix::refs::FullName::try_from(reference) else {
        return Ok(None);
    };
    for _ in 0..=MAX_SYMBOLIC_DEPTH {
        let Some(found) = repo
            .try_find_reference(&name)
            .map_err(|e| git_failure("tags", &e))?
        else {
            return Ok(None);
        };
        match found.target() {
            gix::refs::TargetRef::Object(id) => return Ok(Some(id.to_owned())),
            gix::refs::TargetRef::Symbolic(next) => name = next.to_owned(),
        }
    }
    Ok(None)
}

/// The commit a tag pointing at `tip` names, and the tag objects on the way
/// there. A tag naming a tree or blob is refused: a push carries commits.
fn tag_chain(
    repo: &gix::Repository,
    tip: ObjectId,
    tag: &str,
) -> Result<(ObjectId, Vec<ObjectId>), RemoteError> {
    let mut chain = vec![];
    let mut at = tip;
    loop {
        let object = repo
            .try_find_object(at)
            .map_err(|e| host(git_failure("tags", &e)))?
            .ok_or_else(|| RemoteError::NoTag(tag.to_owned()))?;
        match object.kind {
            gix::object::Kind::Commit => return Ok((at, chain)),
            gix::object::Kind::Tag if chain.len() < MAX_TAG_CHAIN => {
                chain.push(at);
                at = object
                    .into_tag()
                    .target_id()
                    .map_err(|e| host(git_failure("tags", &e)))?
                    .detach();
            }
            _ => return Err(RemoteError::TagTarget(tag.to_owned())),
        }
    }
}

/// The current branch's short name.
fn current_branch(repo: &gix::Repository) -> Result<String, RemoteError> {
    head_branch(repo)?.ok_or_else(|| host(unsupported("HEAD is not on a branch; name the branch")))
}

/// The current branch's short name, or `None` when HEAD is detached. A
/// HEAD that cannot be read is an error.
pub(super) fn head_branch(repo: &gix::Repository) -> Result<Option<String>, RemoteError> {
    Ok(repo
        .head_name()
        .map_err(|e| host(git_failure("head", &e)))?
        .and_then(|name| {
            name.category_and_short_name()
                .map(|(_, short)| short.to_string())
        }))
}

/// An HTTPS repository URL (or loopback HTTP, for a forge on this computer)
/// that carries no credentials, query or fragment.
pub(super) fn remote_url(configured: &str) -> Result<url::Url, RemoteError> {
    let url = url::Url::parse(configured).map_err(|_| RemoteError::RemoteUrl)?;
    let loopback = matches!(
        url.host(),
        Some(url::Host::Domain("localhost"))
            | Some(url::Host::Ipv4(std::net::Ipv4Addr::LOCALHOST))
            | Some(url::Host::Ipv6(std::net::Ipv6Addr::LOCALHOST))
    );
    if !(url.scheme() == "https" || url.scheme() == "http" && loopback) || url.host().is_none() {
        return Err(RemoteError::RemoteUrl);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(RemoteError::RemoteCredentials);
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(RemoteError::RemoteUrl);
    }
    Ok(url)
}

/// What one push sends.
pub(crate) struct Packed {
    /// The server's object for the reference, or the null id if it has none.
    pub old: ObjectId,
    pub pack: Vec<u8>,
    pub commits: usize,
    pub objects: usize,
}

/// The pack that brings the server's branch or tag from what it advertised
/// to `target.tip`, or `None` when the server already has the tip there.
/// A branch is refused unless the server's commit is an ancestor of the
/// tip; a tag is refused if the server has it at all, as git refuses to
/// replace a tag without force.
pub(crate) fn pack(
    root: PathBuf,
    target: &PushTarget,
    advertised: &Advertisement,
    limits: Limits,
    interrupt: &AtomicBool,
) -> Result<Option<Packed>, RemoteError> {
    let old = advertised
        .get(&target.reference())
        .unwrap_or_else(|| ObjectId::null(target.hash));
    let repo = open(root, interrupt).map_err(host)?;
    // Checked before anything else: a reference that moved since it was
    // resolved is not up to date merely because the server has its old tip.
    let now = match target.kind {
        PushKind::Branch => peeled(&repo, &target.reference()),
        PushKind::Tag => direct(&repo, &target.reference()),
    };
    if now.map_err(host)? != Some(target.tip) {
        return Err(RemoteError::Moved(target.label()));
    }
    if old == target.tip {
        return Ok(None);
    }
    if !old.is_null() {
        if target.kind == PushKind::Tag {
            return Err(RemoteError::TagExists(target.name.clone()));
        }
        match ancestor(&repo, old, target.tip, MAX_ANCESTRY, interrupt).map_err(host)? {
            Some(true) => {}
            Some(false) => return Err(RemoteError::NotFastForward(target.name.clone())),
            None => return Err(RemoteError::History),
        }
    }
    let mut chosen =
        a206_git_push::objects(&repo, target.commit, advertised.known(), limits, interrupt)?;
    if chosen.ids.len() + target.tags.len() > limits.objects {
        return Err(PackError::TooManyObjects(limits.objects).into());
    }
    chosen.ids.extend(&target.tags);
    let pack = a206_git_push::write_pack(&repo, &chosen.ids, limits, interrupt)?;
    Ok(Some(Packed {
        old,
        pack,
        commits: chosen.commits,
        objects: chosen.ids.len(),
    }))
}

/// The commit `reference` points at, if it exists.
pub(super) fn peeled(
    repo: &gix::Repository,
    reference: &str,
) -> Result<Option<ObjectId>, HostFailure> {
    Ok(repo
        .try_find_reference(reference)
        .map_err(|e| git_failure("branches", &e))?
        .and_then(|mut reference| reference.peel_to_id().ok())
        .map(|id| id.detach()))
}

/// Whether `old` is `tip` or one of its ancestors, or `None` when that
/// takes more than `limit` commits to tell. A commit the repository lacks
/// is not.
pub(super) fn ancestor(
    repo: &gix::Repository,
    old: ObjectId,
    tip: ObjectId,
    limit: usize,
    interrupt: &AtomicBool,
) -> Result<Option<bool>, HostFailure> {
    if !repo.has_object(old) {
        return Ok(Some(false));
    }
    let walk = repo
        .rev_walk([tip])
        .all()
        .map_err(|e| git_failure("log", &e))?;
    for (seen, info) in walk.enumerate() {
        if seen == limit {
            return Ok(None);
        }
        if seen % 1024 == 0 {
            check(interrupt)?;
        }
        if info.map_err(|e| git_failure("log", &e))?.id == old {
            return Ok(Some(true));
        }
    }
    Ok(Some(false))
}

/// What became of the remote-tracking branch after a push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tracked {
    /// It now points at the pushed commit.
    Updated,
    /// It already pointed at a commit the pushed one does not descend from,
    /// such as one a later push or fetch recorded, and was left there. A
    /// commit too far back to tell is reported as a failure instead.
    Kept,
}

/// Point the remote-tracking branch at what the server now has, as git does
/// after a push. Only moves it forward: the update is refused if the branch
/// changed after it was read, so two pushes finishing out of order cannot
/// rewind it.
pub(crate) fn record(root: PathBuf, target: &PushTarget) -> Result<Tracked, HostFailure> {
    record_within(root, target, MAX_ANCESTRY)
}

/// [`record`], walking at most `limit` commits to compare the two.
fn record_within(root: PathBuf, target: &PushTarget, limit: usize) -> Result<Tracked, HostFailure> {
    let never = AtomicBool::new(false);
    let repo = open(root, &never)?;
    let settings = settings(&repo)?;
    let (name, email) = settings.identity()?;
    let tracking = format!("refs/remotes/{}/{}", target.remote, target.name);
    let full: gix::refs::FullName = tracking.as_str().try_into().map_err(|_| HostFailure {
        code: HostErrorCode::Invalid,
        message: "the remote-tracking branch name is not valid".into(),
    })?;
    let signature = gix::actor::Signature {
        name,
        email,
        time: gix::date::Time::now_local_or_utc(),
    };
    let mut attempt = 0;
    loop {
        let current = peeled(&repo, &tracking)?;
        let expected = match current {
            None => PreviousValue::MustNotExist,
            Some(current) if current == target.tip => return Ok(Tracked::Updated),
            Some(current) => match ancestor(&repo, current, target.tip, limit, &never)? {
                Some(true) => PreviousValue::MustExistAndMatch(Target::Object(current)),
                Some(false) => return Ok(Tracked::Kept),
                None => {
                    return Err(unsupported(
                        "the remote-tracking branch is too far behind the pushed commit to tell \
                         whether it is older; fetch the remote to update it",
                    ));
                }
            },
        };
        let edit = RefEdit {
            change: Change::Update {
                log: LogChange {
                    mode: RefLog::AndReference,
                    force_create_reflog: false,
                    message: "update by push".into(),
                },
                expected,
                new: Target::Object(target.tip),
            },
            name: full.clone(),
            deref: false,
        };
        let mut time = Default::default();
        match repo.edit_references_as([edit], Some(signature.to_ref(&mut time))) {
            Ok(_) => return Ok(Tracked::Updated),
            // Another update got there first: decide again from its value.
            Err(_) if attempt < TRACKING_ATTEMPTS && peeled(&repo, &tracking)? != current => {
                attempt += 1;
            }
            Err(e) => return Err(git_failure("push", &e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nonce() -> String {
        uuid::Uuid::new_v4().simple().to_string()
    }
    /// A repository with one commit on its current branch and `config`
    /// appended to its configuration.
    fn project(config: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let repo = gix::init(dir.path()).unwrap();
        let blob = repo.write_blob(nonce().as_bytes()).unwrap().detach();
        let mut tree = repo
            .edit_tree(ObjectId::empty_tree(repo.object_hash()))
            .unwrap();
        tree.upsert("file", gix::objs::tree::EntryKind::Blob, blob)
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
            "HEAD",
            nonce(),
            tree,
            None::<ObjectId>,
        )
        .unwrap();
        let path = dir.path().join(".git/config");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("[user]\n\tname = n\n\temail = n@example.invalid\n");
        text.push_str(config);
        std::fs::write(path, text).unwrap();
        dir
    }
    /// A new commit on the current branch, whose parent is its tip.
    fn advance(dir: &tempfile::TempDir) -> ObjectId {
        let repo = gix::open(dir.path()).unwrap();
        let parent = repo.head_id().unwrap().detach();
        let tree = repo.head_tree_id().unwrap().detach();
        let signature = gix::actor::Signature {
            name: "n".into(),
            email: "n@example.invalid".into(),
            time: gix::date::Time::new(2_000_000, 0),
        };
        let (mut a, mut b) = Default::default();
        repo.commit_as(
            signature.to_ref(&mut a),
            signature.to_ref(&mut b),
            "HEAD",
            nonce(),
            tree,
            [parent],
        )
        .unwrap()
        .detach()
    }
    fn advertising(target: &PushTarget, tip: ObjectId) -> Advertisement {
        Advertisement {
            refs: vec![(target.reference(), tip)],
            haves: vec![],
            capabilities: vec!["report-status".into()],
        }
    }
    const ROOMY: Limits = Limits {
        commits: 100,
        objects: 1_000,
        bytes: 1 << 20,
        object: 1 << 20,
    };
    fn target_of(
        dir: &tempfile::TempDir,
        branch: Option<&str>,
        remote: Option<&str>,
    ) -> Result<PushTarget, RemoteError> {
        target(
            dir.path().into(),
            Pushed::Branch(branch),
            remote,
            &AtomicBool::new(false),
        )
    }

    #[test]
    fn the_branch_and_remote_resolve_as_git_would() {
        let (host, other) = (
            format!("{}.example", nonce()),
            format!("{}.example", nonce()),
        );
        let dir = project(&format!(
            "[remote \"origin\"]\n\turl = https://{host}/a.git\n[remote \"up\"]\n\turl = https://{other}/b.git\n\tpushurl = https://{other}/push.git\n"
        ));
        let current = target_of(&dir, None, None).unwrap();
        assert_eq!(current.remote, "origin");
        assert_eq!(current.url.as_str(), format!("https://{host}/a.git"));
        assert_eq!(current.reference(), format!("refs/heads/{}", current.name));
        let head = gix::open(dir.path()).unwrap().head_id().unwrap().detach();
        assert_eq!(current.tip, head);
        // A push URL wins over the fetch URL.
        let up = target_of(&dir, None, Some("up")).unwrap();
        assert_eq!(up.url.as_str(), format!("https://{other}/push.git"));
        // The branch's configured remote is its default.
        let path = dir.path().join(".git/config");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(&format!("[branch \"{}\"]\n\tremote = up\n", current.name));
        std::fs::write(&path, text).unwrap();
        assert_eq!(target_of(&dir, None, None).unwrap().remote, "up");

        assert!(matches!(
            target_of(&dir, Some(&nonce()), None),
            Err(RemoteError::NoBranch(_))
        ));
        assert!(matches!(
            target_of(&dir, Some("-x"), None),
            Err(RemoteError::BranchName)
        ));
        assert!(matches!(
            target_of(&dir, None, Some(&nonce())),
            Err(RemoteError::NoRemote(_))
        ));
        assert!(matches!(
            target_of(&dir, None, Some("a/../b")),
            Err(RemoteError::RemoteName)
        ));
    }

    #[test]
    fn remotes_git_would_treat_differently_refuse_a_push() {
        let host = format!("{}.example", nonce());
        let cases = [
            (
                format!("[remote \"origin\"]\n\turl = git@{host}:a.git\n"),
                "url",
            ),
            (
                format!("[remote \"origin\"]\n\turl = ssh://{host}/a.git\n"),
                "url",
            ),
            (
                format!("[remote \"origin\"]\n\turl = http://{host}/a.git\n"),
                "url",
            ),
            (
                format!("[remote \"origin\"]\n\turl = https://{host}/a.git?x=1\n"),
                "url",
            ),
            (
                format!("[remote \"origin\"]\n\turl = https://me:secret@{host}/a.git\n"),
                "credentials",
            ),
            (
                format!(
                    "[remote \"origin\"]\n\turl = https://{host}/a.git\n[url \"https://mirror.example/\"]\n\tpushInsteadOf = https://{host}/\n"
                ),
                "rewritten",
            ),
        ];
        for (config, expected) in cases {
            let dir = project(&config);
            let result = target_of(&dir, None, None);
            let matched = match expected {
                "url" => matches!(result, Err(RemoteError::RemoteUrl)),
                "credentials" => matches!(result, Err(RemoteError::RemoteCredentials)),
                _ => matches!(result, Err(RemoteError::Rewritten)),
            };
            assert!(matched, "{config}: {result:?}");
        }

        let dir = project(&format!(
            "[remote \"origin\"]\n\turl = https://{host}/a.git\n"
        ));
        target_of(&dir, None, None).unwrap();
        let hooks = dir.path().join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::write(hooks.join("pre-push"), "#!/bin/sh\n").unwrap();
        assert!(matches!(
            target_of(&dir, None, None),
            Err(RemoteError::Repository(HostFailure {
                code: HostErrorCode::Unsupported,
                ..
            }))
        ));
    }

    #[test]
    fn a_branch_that_moved_is_never_reported_up_to_date() {
        let host = format!("{}.example", nonce());
        let dir = project(&format!(
            "[remote \"origin\"]\n\turl = https://{host}/a.git\n"
        ));
        let resolved = target_of(&dir, None, None).unwrap();
        let never = AtomicBool::new(false);
        let server = advertising(&resolved, resolved.tip);
        assert!(
            pack(dir.path().into(), &resolved, &server, ROOMY, &never)
                .unwrap()
                .is_none()
        );
        // A commit made after the branch was resolved: the server having the
        // old tip does not make the branch up to date.
        advance(&dir);
        assert!(matches!(
            pack(dir.path().into(), &resolved, &server, ROOMY, &never),
            Err(RemoteError::Moved(_))
        ));
    }

    #[test]
    fn the_remote_tracking_branch_only_moves_forward() {
        let host = format!("{}.example", nonce());
        let dir = project(&format!(
            "[remote \"origin\"]\n\turl = https://{host}/a.git\n"
        ));
        let first = target_of(&dir, None, None).unwrap();
        advance(&dir);
        let second = target_of(&dir, None, None).unwrap();
        let tracking = format!("refs/remotes/origin/{}", first.name);
        let at = || {
            let repo = gix::open(dir.path()).unwrap();
            peeled(&repo, &tracking).unwrap()
        };
        assert_eq!(at(), None);
        assert_eq!(record(dir.path().into(), &first).unwrap(), Tracked::Updated);
        assert_eq!(at(), Some(first.tip));
        assert_eq!(
            record(dir.path().into(), &second).unwrap(),
            Tracked::Updated
        );
        assert_eq!(at(), Some(second.tip));
        // The earlier push finishing last does not rewind it.
        assert_eq!(record(dir.path().into(), &first).unwrap(), Tracked::Kept);
        assert_eq!(at(), Some(second.tip));
        // Too far behind to tell is a failure, not a claim that it is newer.
        advance(&dir);
        let third = target_of(&dir, None, None).unwrap();
        assert!(matches!(
            record_within(dir.path().into(), &third, 1),
            Err(HostFailure {
                code: HostErrorCode::Unsupported,
                ..
            })
        ));
        assert_eq!(at(), Some(second.tip));
        assert_eq!(
            record_within(dir.path().into(), &third, 2).unwrap(),
            Tracked::Updated
        );
        assert_eq!(at(), Some(third.tip));
    }

    /// An annotated tag object naming `target`, written to the repository.
    fn tag_object(repo: &gix::Repository, target: ObjectId, kind: gix::object::Kind) -> ObjectId {
        repo.write_object(gix::objs::Tag {
            target,
            target_kind: kind,
            name: nonce().into(),
            tagger: None,
            message: nonce().into(),
            signature: None,
        })
        .unwrap()
        .detach()
    }
    fn tag_ref(repo: &gix::Repository, name: &str, target: gix::refs::Target) {
        repo.edit_reference(RefEdit {
            change: Change::Update {
                log: Default::default(),
                expected: PreviousValue::MustNotExist,
                new: target,
            },
            name: format!("refs/tags/{name}").try_into().unwrap(),
            deref: false,
        })
        .unwrap();
    }
    fn tag_target(dir: &tempfile::TempDir, tag: &str) -> Result<PushTarget, RemoteError> {
        target(
            dir.path().into(),
            Pushed::Tag(tag),
            None,
            &AtomicBool::new(false),
        )
    }

    #[test]
    fn a_tag_push_sends_its_tag_objects_and_never_replaces_a_server_tag() {
        let host = format!("{}.example", nonce());
        let dir = project(&format!(
            "[remote \"origin\"]\n\turl = https://{host}/a.git\n"
        ));
        let repo = gix::open(dir.path()).unwrap();
        let head = repo.head_id().unwrap().detach();
        let (plain, annotated, nested, symbolic, of_blob, broken) =
            (nonce(), nonce(), nonce(), nonce(), nonce(), nonce());
        tag_ref(&repo, &plain, Target::Object(head));
        let object = tag_object(&repo, head, gix::object::Kind::Commit);
        tag_ref(&repo, &annotated, Target::Object(object));
        let outer = tag_object(&repo, object, gix::object::Kind::Tag);
        tag_ref(&repo, &nested, Target::Object(outer));
        tag_ref(
            &repo,
            &symbolic,
            Target::Symbolic(format!("refs/tags/{annotated}").try_into().unwrap()),
        );
        let blob = repo.write_blob(nonce().as_bytes()).unwrap().detach();
        tag_ref(&repo, &of_blob, Target::Object(blob));
        let missing =
            ObjectId::from_hex(format!("{}{}", nonce(), &nonce()[..8]).as_bytes()).unwrap();
        tag_ref(&repo, &broken, Target::Object(missing));

        let resolved = tag_target(&dir, &plain).unwrap();
        assert_eq!(resolved.kind, PushKind::Tag);
        assert_eq!(resolved.reference(), format!("refs/tags/{plain}"));
        assert_eq!((resolved.tip, resolved.commit), (head, head));
        assert!(resolved.tags.is_empty());
        assert_eq!(resolved.remote, "origin");
        let resolved = tag_target(&dir, &nested).unwrap();
        assert_eq!((resolved.tip, resolved.commit), (outer, head));
        assert_eq!(resolved.tags, vec![outer, object]);
        // A symbolic tag pushes the object its target names, under its own
        // name.
        let resolved = tag_target(&dir, &symbolic).unwrap();
        assert_eq!(resolved.reference(), format!("refs/tags/{symbolic}"));
        assert_eq!(
            (resolved.tip, resolved.tags.clone()),
            (object, vec![object])
        );
        // One whose target's name is not UTF-8 follows that exact name, not
        // a lossy copy of it, even when the copy names another tag.
        {
            use std::os::unix::ffi::OsStrExt;
            let (bytes, lossy) = (nonce(), nonce());
            let mut raw = format!("{bytes}-").into_bytes();
            raw.push(0xff);
            let tags = dir.path().join(".git/refs/tags");
            std::fs::write(
                tags.join(std::ffi::OsStr::from_bytes(&raw)),
                format!("{object}\n"),
            )
            .unwrap();
            tag_ref(&repo, &format!("{bytes}-\u{fffd}"), Target::Object(head));
            let mut pointer = b"ref: refs/tags/".to_vec();
            pointer.extend_from_slice(&raw);
            pointer.push(b'\n');
            std::fs::write(tags.join(&lossy), pointer).unwrap();
            assert_eq!(tag_target(&dir, &lossy).unwrap().tip, object);
        }

        assert!(matches!(
            tag_target(&dir, &of_blob),
            Err(RemoteError::TagTarget(_))
        ));
        assert!(matches!(
            tag_target(&dir, &broken),
            Err(RemoteError::NoTag(_))
        ));
        assert!(matches!(
            tag_target(&dir, &nonce()),
            Err(RemoteError::NoTag(_))
        ));
        // An object that is there but cannot be read is a failure, not a
        // missing tag.
        let (unreadable, corrupt) = (nonce(), format!("{}{}", nonce(), &nonce()[..8]));
        let loose = dir.path().join(".git/objects").join(&corrupt[..2]);
        std::fs::create_dir_all(&loose).unwrap();
        std::fs::write(loose.join(&corrupt[2..]), nonce()).unwrap();
        let corrupt = ObjectId::from_hex(corrupt.as_bytes()).unwrap();
        tag_ref(&repo, &unreadable, Target::Object(corrupt));
        assert!(matches!(
            tag_target(&dir, &unreadable),
            Err(RemoteError::Repository(_))
        ));
        assert!(matches!(tag_target(&dir, "-x"), Err(RemoteError::TagName)));

        // A server without the tag gets the tag objects with the commit.
        let never = AtomicBool::new(false);
        let empty = Advertisement {
            refs: vec![],
            haves: vec![],
            capabilities: vec!["report-status".into()],
        };
        let nested_target = tag_target(&dir, &nested).unwrap();
        let packed = pack(dir.path().into(), &nested_target, &empty, ROOMY, &never)
            .unwrap()
            .unwrap();
        assert_eq!(packed.old, ObjectId::null(nested_target.hash));
        assert_eq!(packed.commits, 1);
        // The commit, its tree and blob, and both tag objects.
        assert_eq!(packed.objects, 5);
        // One that already has the commit gets only the tag objects.
        let knows_commit = advertising(&tag_target(&dir, &plain).unwrap(), head);
        let packed = pack(
            dir.path().into(),
            &nested_target,
            &knows_commit,
            ROOMY,
            &never,
        )
        .unwrap()
        .unwrap();
        assert_eq!((packed.commits, packed.objects), (0, 2));
        // The same tag is up to date; a different one is never replaced.
        assert!(
            pack(
                dir.path().into(),
                &nested_target,
                &advertising(&nested_target, outer),
                ROOMY,
                &never
            )
            .unwrap()
            .is_none()
        );
        assert!(matches!(
            pack(
                dir.path().into(),
                &nested_target,
                &advertising(&nested_target, object),
                ROOMY,
                &never
            ),
            Err(RemoteError::TagExists(_))
        ));
        // A tag moved after it was resolved is not pushed.
        let annotated_target = tag_target(&dir, &annotated).unwrap();
        repo.edit_reference(RefEdit {
            change: Change::Update {
                log: Default::default(),
                expected: PreviousValue::Any,
                new: Target::Object(head),
            },
            name: format!("refs/tags/{annotated}").try_into().unwrap(),
            deref: false,
        })
        .unwrap();
        assert!(matches!(
            pack(dir.path().into(), &annotated_target, &empty, ROOMY, &never),
            Err(RemoteError::Moved(_))
        ));
    }

    #[test]
    fn a_repository_whose_configuration_includes_files_is_not_pushed() {
        let host = format!("{}.example", nonce());
        let outside = tempfile::tempdir().unwrap();
        let included = outside.path().join("config");
        std::fs::write(
            &included,
            format!("[remote \"origin\"]\n\turl = https://{host}/a.git\n"),
        )
        .unwrap();
        for section in ["include", "includeIf \"gitdir:/\""] {
            let dir = project(&format!("[{section}]\n\tpath = {}\n", included.display()));
            assert!(
                matches!(
                    target_of(&dir, None, None),
                    Err(RemoteError::Repository(HostFailure {
                        code: HostErrorCode::Unsupported,
                        ..
                    }))
                ),
                "{section}"
            );
        }
    }
}
