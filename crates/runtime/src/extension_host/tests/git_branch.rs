//! The git branch host calls, against repositories built with gitoxide.
use super::git::{repository, unique};
use super::git_commit::{configure, head, project, writer};
use super::*;
use a206_extensions::abi::GitBranch;
use std::path::Path;

fn create(name: &str, start: Option<&str>) -> HostRequest {
    HostRequest::GitCreateBranch {
        name: name.into(),
        start: start.map(Into::into),
    }
}
async fn branches(host: &mut HostCalls) -> Vec<GitBranch> {
    let reply = host.handle(HostRequest::GitBranches).await;
    let HostReply::Branches(listing) = reply else {
        panic!("listing failed: {reply:?}");
    };
    assert!(!listing.truncated);
    listing.branches
}
/// A repository with one commit and a committer for reflogs.
fn branchable(root: &Path) {
    repository(root, &[("a.txt", "one\n")]);
    configure(root, "[user]\n\tname = n\n\temail = n@example.invalid\n");
}
fn head_ref(root: &Path) -> Option<String> {
    gix::open(root)
        .unwrap()
        .head_name()
        .unwrap()
        .map(|name| name.as_bstr().to_string())
}

#[tokio::test]
async fn branches_are_created_without_switching_to_them() {
    let (dir, store) = store();
    let root = &project(dir.path());
    branchable(root);
    let mut host = writer(&store, root);
    let first = head(root).unwrap().to_string();
    let [current] = branches(&mut host).await.try_into().unwrap();
    assert!(current.current);
    assert_eq!(current.commit, first);
    let (on, index) = (head_ref(root), std::fs::read(root.join(".git/index")).ok());

    let name = format!("feature/{}", unique("b"));
    let reply = host.handle(create(&name, None)).await;
    let HostReply::Branch(made) = reply else {
        panic!("creation failed: {reply:?}");
    };
    assert_eq!(
        (made.name.as_str(), made.commit.as_str()),
        (name.as_str(), first.as_str())
    );
    // Nothing but the new reference and its reflog changed.
    assert_eq!(head_ref(root), on);
    assert_eq!(std::fs::read(root.join(".git/index")).ok(), index);
    let log = std::fs::read_to_string(root.join(".git/logs/refs/heads").join(&name)).unwrap();
    assert!(log.contains("branch: Created from HEAD"), "{log}");

    // A later commit on the current branch, then branches from a branch
    // and from a full commit id.
    let repo = gix::open(root).unwrap();
    let tree = repo.head_tree_id().unwrap().detach();
    let signature = gix::actor::Signature {
        name: "n".into(),
        email: "n@example.invalid".into(),
        time: gix::date::Time::new(2_000_000, 0),
    };
    let (mut a, mut b) = Default::default();
    let second = repo
        .commit_as(
            signature.to_ref(&mut a),
            signature.to_ref(&mut b),
            "HEAD",
            unique("second"),
            tree,
            [head(root).unwrap()],
        )
        .unwrap()
        .to_string();
    let from_branch = unique("from-branch");
    let from_id = unique("from-id");
    assert!(matches!(
        host.handle(create(&from_branch, Some(&name))).await,
        HostReply::Branch(GitBranch { commit, .. }) if commit == first
    ));
    assert!(matches!(
        host.handle(create(&from_id, Some(&second))).await,
        HostReply::Branch(GitBranch { commit, .. }) if commit == second
    ));
    let listed = branches(&mut host).await;
    let commit_of = |wanted: &str| {
        listed
            .iter()
            .find(|branch| branch.name == wanted)
            .map(|branch| (branch.commit.clone(), branch.current))
    };
    assert_eq!(commit_of(&name), Some((first.clone(), false)));
    assert_eq!(commit_of(&from_branch), Some((first, false)));
    assert_eq!(commit_of(&from_id), Some((second.clone(), false)));
    assert_eq!(commit_of(&current.name), Some((second, true)));
    assert!(listed.windows(2).all(|pair| pair[0].name < pair[1].name));
}

#[tokio::test]
async fn refused_branches_create_nothing() {
    let (dir, store) = store();
    let root = &project(dir.path());
    branchable(root);
    let mut host = writer(&store, root);
    let existing = branches(&mut host).await[0].name.clone();
    let id = head(root).unwrap().to_string();
    for (name, start, expected) in [
        ("", None, HostErrorCode::Invalid),
        ("-x", None, HostErrorCode::Invalid),
        ("HEAD", None, HostErrorCode::Invalid),
        ("a..b", None, HostErrorCode::Invalid),
        ("a b", None, HostErrorCode::Invalid),
        ("x.lock", None, HostErrorCode::Invalid),
        ("x/", None, HostErrorCode::Invalid),
        (existing.as_str(), None, HostErrorCode::Conflict),
        ("new", Some("missing"), HostErrorCode::NotFound),
        // Revision expressions and short ids are not evaluated.
        ("new", Some("HEAD~0"), HostErrorCode::NotFound),
        ("new", Some(&id[..12]), HostErrorCode::NotFound),
    ] {
        assert_eq!(
            code(host.handle(create(name, start)).await),
            Some(expected),
            "{name:?} {start:?}"
        );
    }
    assert_eq!(branches(&mut host).await.len(), 1);

    // Branches that would nest with an existing one, either way round.
    let parent = unique("p");
    assert!(matches!(
        host.handle(create(&parent, None)).await,
        HostReply::Branch(_)
    ));
    for name in [format!("{parent}/child"), existing.clone() + "/x"] {
        assert_eq!(
            code(host.handle(create(&name, None)).await),
            Some(HostErrorCode::Conflict),
            "{name}"
        );
    }
    let nested = format!("{}/leaf", unique("folder"));
    assert!(matches!(
        host.handle(create(&nested, None)).await,
        HostReply::Branch(_)
    ));
    let folder = nested.split('/').next().unwrap().to_string();
    assert_eq!(
        code(host.handle(create(&folder, None)).await),
        Some(HostErrorCode::Conflict)
    );
    assert_eq!(branches(&mut host).await.len(), 3);
}

#[tokio::test]
async fn long_nested_names_are_accepted_to_a_depth() {
    let (dir, store) = store();
    let root = &project(dir.path());
    branchable(root);
    let mut host = writer(&store, root);
    // The documented limit is 20 folders. Longer than 255 bytes in all,
    // as git allows.
    let folder = unique("f");
    let deepest = format!("{}{}", format!("{folder}/").repeat(20), unique("b"));
    assert!(deepest.len() > 255);
    let reply = host.handle(create(&deepest, None)).await;
    assert!(matches!(reply, HostReply::Branch(_)), "{reply:?}");
    let deeper = format!("{}{}", "a/".repeat(21), unique("b"));
    assert_eq!(
        code(host.handle(create(&deeper, None)).await),
        Some(HostErrorCode::Invalid)
    );
    assert_eq!(branches(&mut host).await.len(), 2);
}

#[tokio::test]
async fn an_unborn_current_branch_is_not_given_a_commit() {
    let (dir, store) = store();
    let root = &project(dir.path());
    branchable(root);
    let mut host = writer(&store, root);
    let main = branches(&mut host).await[0].name.clone();
    let orphan = unique("orphan");
    std::fs::write(
        root.join(".git/HEAD"),
        format!("ref: refs/heads/{orphan}\n"),
    )
    .unwrap();
    assert_eq!(
        code(host.handle(create(&orphan, Some(&main))).await),
        Some(HostErrorCode::Conflict)
    );
    assert_eq!(head(root), None);
    assert_eq!(branches(&mut host).await.len(), 1);
}

#[tokio::test]
async fn branches_need_their_grants() {
    let (dir, store) = store();
    let root = &project(dir.path());
    branchable(root);
    let mut none = workspace_calls(&store, Some(root.into()), &[Capability::ReadWorkspace]);
    assert_eq!(
        code(none.handle(HostRequest::GitBranches).await),
        Some(HostErrorCode::Denied)
    );
    let mut reader = workspace_calls(&store, Some(root.into()), &[Capability::GitRead]);
    assert_eq!(branches(&mut reader).await.len(), 1);
    assert_eq!(
        code(reader.handle(create(&unique("b"), None)).await),
        Some(HostErrorCode::Denied)
    );
    assert_eq!(branches(&mut reader).await.len(), 1);
}

#[tokio::test]
async fn protections_git_would_apply_refuse_a_branch() {
    for setup in ["hook", "no identity", "no commit"] {
        let (dir, store) = store();
        let root = &project(dir.path());
        let expected = match setup {
            "hook" => {
                branchable(root);
                std::fs::write(root.join(".git/hooks/reference-transaction"), "").unwrap();
                HostErrorCode::Unsupported
            }
            "no identity" => {
                repository(root, &[("a.txt", "one\n")]);
                HostErrorCode::Unsupported
            }
            _ => {
                gix::init(root).unwrap();
                configure(root, "[user]\n\tname = n\n\temail = n@example.invalid\n");
                HostErrorCode::Invalid
            }
        };
        let mut host = writer(&store, root);
        assert_eq!(
            code(host.handle(create(&unique("b"), None)).await),
            Some(expected),
            "{setup}"
        );
        let refs = gix::open(root)
            .unwrap()
            .references()
            .unwrap()
            .local_branches()
            .unwrap()
            .count();
        assert_eq!(refs, usize::from(setup != "no commit"), "{setup}");
    }
}
