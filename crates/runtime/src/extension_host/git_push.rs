//! The repository side of the native `git_push` tool (see
//! [`crate::git_push`]): which branch goes where, the objects it sends and
//! the remote-tracking branch it updates. The project's repository is opened
//! as the git host calls open it, and nothing it configures runs; where git
//! would run a program or rewrite the remote's URL, the push is refused.
use super::{
    git::{check, git_failure, open, unsupported},
    git_branch::branch_name,
    git_commit::{Settings, UserGit, has_hooks},
};
use crate::git_push::PushError;
use a206_extensions::abi::{HostErrorCode, HostFailure};
use a206_git_push::{Advertisement, Limits};
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
const MAX_ANCESTRY: usize = 100_000;

/// A branch to push and where to.
#[derive(Debug, Clone)]
pub(crate) struct PushTarget {
    /// The local branch, such as `feature/login`.
    pub branch: String,
    /// The commit it points at.
    pub tip: ObjectId,
    pub remote: String,
    /// The repository URL the remote pushes to.
    pub url: url::Url,
    pub hash: gix::hash::Kind,
}
impl PushTarget {
    /// The reference the branch updates on the server, by the same name.
    pub fn reference(&self) -> String {
        format!("refs/heads/{}", self.branch)
    }
}

fn host(error: HostFailure) -> PushError {
    PushError::Repository(error)
}

/// Resolve `branch` (default: the current one) and `remote` (default: the
/// branch's configured remote, else `origin`) in the project at `root`.
pub(crate) fn target(
    root: PathBuf,
    branch: Option<&str>,
    remote: Option<&str>,
    interrupt: &AtomicBool,
) -> Result<PushTarget, PushError> {
    let repo = open(root, interrupt).map_err(host)?;
    let user = UserGit::load().map_err(host)?;
    let settings = Settings::resolve(&repo, &user).map_err(host)?;
    if has_hooks(&repo, &settings, &PUSH_HOOKS) {
        return Err(host(unsupported(
            "the repository has a pre-push or reference-transaction hook, which git would run",
        )));
    }
    // The remote-tracking branch's reflog records who pushed.
    settings.identity().map_err(host)?;
    let branch = match branch {
        Some(name) => name.to_owned(),
        None => repo
            .head_name()
            .map_err(|e| host(git_failure("head", &e)))?
            .and_then(|name| {
                name.category_and_short_name()
                    .map(|(_, short)| short.to_string())
            })
            .ok_or_else(|| {
                host(unsupported(
                    "HEAD is not on a branch; name the branch to push",
                ))
            })?,
    };
    let full = branch_name(&branch).ok_or(PushError::BranchName)?;
    let tip = repo
        .try_find_reference(&full)
        .map_err(|e| host(git_failure("branches", &e)))?
        .ok_or_else(|| PushError::NoBranch(branch.clone()))?
        .peel_to_id()
        .map_err(|_| PushError::NoBranch(branch.clone()))?
        .detach();
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
        return Err(PushError::RemoteName);
    }
    let configured = settings
        .string(&format!("remote.{remote}.pushurl"))
        .or_else(|| settings.string(&format!("remote.{remote}.url")))
        .ok_or_else(|| PushError::NoRemote(remote.clone()))?;
    let configured = std::str::from_utf8(&configured).map_err(|_| PushError::RemoteUrl)?;
    if settings.rewrites(configured) {
        return Err(PushError::Rewritten);
    }
    let url = remote_url(configured)?;
    Ok(PushTarget {
        branch,
        tip,
        remote,
        url,
        hash: repo.object_hash(),
    })
}

/// An HTTPS repository URL (or loopback HTTP, for a forge on this computer)
/// that carries no credentials, query or fragment.
fn remote_url(configured: &str) -> Result<url::Url, PushError> {
    let url = url::Url::parse(configured).map_err(|_| PushError::RemoteUrl)?;
    let loopback = matches!(
        url.host(),
        Some(url::Host::Domain("localhost"))
            | Some(url::Host::Ipv4(std::net::Ipv4Addr::LOCALHOST))
            | Some(url::Host::Ipv6(std::net::Ipv6Addr::LOCALHOST))
    );
    if !(url.scheme() == "https" || url.scheme() == "http" && loopback) || url.host().is_none() {
        return Err(PushError::RemoteUrl);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(PushError::RemoteCredentials);
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(PushError::RemoteUrl);
    }
    Ok(url)
}

/// What one push sends.
pub(crate) struct Packed {
    /// The server's commit for the branch, or the null id if it has none.
    pub old: ObjectId,
    pub pack: Vec<u8>,
    pub commits: usize,
    pub objects: usize,
}

/// The pack that brings the server's branch from what it advertised to
/// `target.tip`, or `None` when the server already has the tip there.
/// Refused unless the server's commit is an ancestor of the tip.
pub(crate) fn pack(
    root: PathBuf,
    target: &PushTarget,
    advertised: &Advertisement,
    limits: Limits,
    interrupt: &AtomicBool,
) -> Result<Option<Packed>, PushError> {
    let old = advertised
        .get(&target.reference())
        .unwrap_or_else(|| ObjectId::null(target.hash));
    if old == target.tip {
        return Ok(None);
    }
    let repo = open(root, interrupt).map_err(host)?;
    let current = repo
        .try_find_reference(target.reference().as_str())
        .map_err(|e| host(git_failure("branches", &e)))?
        .and_then(|mut reference| reference.peel_to_id().ok())
        .map(|id| id.detach());
    if current != Some(target.tip) {
        return Err(PushError::Moved(target.branch.clone()));
    }
    if !old.is_null() && !ancestor(&repo, old, target.tip, interrupt)? {
        return Err(PushError::NotFastForward(target.branch.clone()));
    }
    check(interrupt).map_err(host)?;
    let chosen = a206_git_push::objects(&repo, target.tip, advertised.known(), limits)?;
    check(interrupt).map_err(host)?;
    let pack = a206_git_push::write_pack(&repo, &chosen.ids, limits.bytes)?;
    Ok(Some(Packed {
        old,
        pack,
        commits: chosen.commits,
        objects: chosen.ids.len(),
    }))
}

/// Whether `old` is `tip` or one of its ancestors. A commit the repository
/// lacks is not.
fn ancestor(
    repo: &gix::Repository,
    old: ObjectId,
    tip: ObjectId,
    interrupt: &AtomicBool,
) -> Result<bool, PushError> {
    if !repo.has_object(old) {
        return Ok(false);
    }
    let walk = repo
        .rev_walk([tip])
        .all()
        .map_err(|e| host(git_failure("log", &e)))?;
    for (seen, info) in walk.enumerate() {
        if seen == MAX_ANCESTRY {
            return Err(PushError::History);
        }
        if seen % 1024 == 0 {
            check(interrupt).map_err(host)?;
        }
        if info.map_err(|e| host(git_failure("log", &e)))?.id == old {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Point the remote-tracking branch at what the server now has, as git does
/// after a push.
pub(crate) fn record(root: PathBuf, target: &PushTarget) -> Result<(), HostFailure> {
    let never = AtomicBool::new(false);
    let repo = open(root, &never)?;
    let user = UserGit::load()?;
    let settings = Settings::resolve(&repo, &user)?;
    let (name, email) = settings.identity()?;
    let tracking = format!("refs/remotes/{}/{}", target.remote, target.branch);
    let full = tracking.as_str().try_into().map_err(|_| HostFailure {
        code: HostErrorCode::Invalid,
        message: "the remote-tracking branch name is not valid".into(),
    })?;
    let signature = gix::actor::Signature {
        name,
        email,
        time: gix::date::Time::now_local_or_utc(),
    };
    let mut time = Default::default();
    let edit = RefEdit {
        change: Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: "update by push".into(),
            },
            expected: PreviousValue::Any,
            new: Target::Object(target.tip),
        },
        name: full,
        deref: false,
    };
    repo.edit_references_as([edit], Some(signature.to_ref(&mut time)))
        .map(drop)
        .map_err(|e| git_failure("push", &e))
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
    fn target_of(
        dir: &tempfile::TempDir,
        branch: Option<&str>,
        remote: Option<&str>,
    ) -> Result<PushTarget, PushError> {
        target(dir.path().into(), branch, remote, &AtomicBool::new(false))
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
        assert_eq!(
            current.reference(),
            format!("refs/heads/{}", current.branch)
        );
        let head = gix::open(dir.path()).unwrap().head_id().unwrap().detach();
        assert_eq!(current.tip, head);
        // A push URL wins over the fetch URL.
        let up = target_of(&dir, None, Some("up")).unwrap();
        assert_eq!(up.url.as_str(), format!("https://{other}/push.git"));
        // The branch's configured remote is its default.
        let path = dir.path().join(".git/config");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(&format!("[branch \"{}\"]\n\tremote = up\n", current.branch));
        std::fs::write(&path, text).unwrap();
        assert_eq!(target_of(&dir, None, None).unwrap().remote, "up");

        assert!(matches!(
            target_of(&dir, Some(&nonce()), None),
            Err(PushError::NoBranch(_))
        ));
        assert!(matches!(
            target_of(&dir, Some("-x"), None),
            Err(PushError::BranchName)
        ));
        assert!(matches!(
            target_of(&dir, None, Some(&nonce())),
            Err(PushError::NoRemote(_))
        ));
        assert!(matches!(
            target_of(&dir, None, Some("a/../b")),
            Err(PushError::RemoteName)
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
                "url" => matches!(result, Err(PushError::RemoteUrl)),
                "credentials" => matches!(result, Err(PushError::RemoteCredentials)),
                _ => matches!(result, Err(PushError::Rewritten)),
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
            Err(PushError::Repository(HostFailure {
                code: HostErrorCode::Unsupported,
                ..
            }))
        ));
    }
}
