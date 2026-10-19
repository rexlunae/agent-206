//! The git tag host calls, against repositories built with gitoxide.
use super::git::{repository, unique};
use super::git_commit::{configure, head, project, writer};
use super::*;
use a206_extensions::abi::GitTag;
use std::path::Path;

fn create(name: &str, target: Option<&str>, message: Option<&str>) -> HostRequest {
    HostRequest::GitCreateTag {
        name: name.into(),
        target: target.map(Into::into),
        message: message.map(Into::into),
    }
}
async fn tags(host: &mut HostCalls) -> Vec<GitTag> {
    let reply = host.handle(HostRequest::GitTags).await;
    let HostReply::Tags(listing) = reply else {
        panic!("listing failed: {reply:?}");
    };
    assert!(!listing.truncated);
    listing.tags
}
/// A repository with one commit and a tagger.
fn taggable(root: &Path) {
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
async fn plain_and_annotated_tags_name_a_commit_and_change_nothing_else() {
    let (dir, store) = store();
    let root = &project(dir.path());
    taggable(root);
    let mut host = writer(&store, root);
    assert!(tags(&mut host).await.is_empty());
    let first = head(root).unwrap().to_string();
    let (on, index) = (head_ref(root), std::fs::read(root.join(".git/index")).ok());

    let plain = format!("release/{}", unique("v"));
    let reply = host.handle(create(&plain, None, None)).await;
    let HostReply::Tag(made) = reply else {
        panic!("creation failed: {reply:?}");
    };
    assert_eq!(
        made,
        GitTag {
            name: plain.clone(),
            commit: Some(first.clone()),
            annotated: false
        }
    );
    let repo = gix::open(root).unwrap();
    let reference = repo
        .find_reference(format!("refs/tags/{plain}").as_str())
        .unwrap();
    assert_eq!(reference.target().id().to_string(), first);

    // An annotated tag, from a branch name, is a tag object naming the
    // commit, with the tagger and message given.
    let branch = repo.head_name().unwrap().unwrap().shorten().to_string();
    let (annotated, message) = (unique("a"), unique("message"));
    let reply = host
        .handle(create(&annotated, Some(&branch), Some(&message)))
        .await;
    assert!(
        matches!(&reply, HostReply::Tag(GitTag { commit: Some(c), annotated: true, .. }) if *c == first),
        "{reply:?}"
    );
    let repo = gix::open(root).unwrap();
    let reference = repo
        .find_reference(format!("refs/tags/{annotated}").as_str())
        .unwrap();
    let object = repo.find_object(reference.target().id()).unwrap();
    let tag = object.try_into_tag().unwrap();
    let decoded = tag.decode().unwrap();
    assert_eq!(decoded.target().to_string(), first);
    assert_eq!(decoded.name, annotated.as_str());
    assert_eq!(decoded.message, format!("{message}\n").as_str());
    assert_eq!(decoded.tagger().unwrap().unwrap().name, "n");
    assert_eq!(head_ref(root), on);
    assert_eq!(std::fs::read(root.join(".git/index")).ok(), index);

    // By full id; and a tag naming a blob is listed without a commit.
    let blob = repo.write_blob(unique("blob")).unwrap().detach();
    repo.reference(
        format!("refs/tags/{}", unique("blob")),
        blob,
        gix::refs::transaction::PreviousValue::MustNotExist,
        "test",
    )
    .unwrap();
    let by_id = unique("id");
    assert!(matches!(
        host.handle(create(&by_id, Some(&first), None)).await,
        HostReply::Tag(_)
    ));
    // A symbolic tag is followed; a tag whose object is missing is still
    // listed.
    let (symbolic, broken) = (unique("sym"), unique("broken"));
    std::fs::write(
        root.join(".git/refs/tags").join(&symbolic),
        format!("ref: refs/tags/{annotated}\n"),
    )
    .unwrap();
    std::fs::write(
        root.join(".git/refs/tags").join(&broken),
        format!("{}\n", "1".repeat(first.len())),
    )
    .unwrap();
    let listed = tags(&mut host).await;
    assert_eq!(listed.len(), 6);
    assert!(listed.windows(2).all(|pair| pair[0].name < pair[1].name));
    let find = |name: &str| listed.iter().find(|tag| tag.name == name).unwrap();
    assert!(find(&annotated).annotated);
    assert_eq!(find(&annotated).commit.as_deref(), Some(first.as_str()));
    assert!(!find(&plain).annotated);
    assert_eq!(
        find(&symbolic),
        &GitTag {
            name: symbolic.clone(),
            ..find(&annotated).clone()
        }
    );
    assert_eq!(
        find(&broken),
        &GitTag {
            name: broken.clone(),
            commit: None,
            annotated: false
        }
    );
    assert_eq!(find(&by_id).commit.as_deref(), Some(first.as_str()));
    assert_eq!(
        listed
            .iter()
            .filter(|tag| tag.commit.is_none() && !tag.annotated)
            .count(),
        2
    );
}

#[tokio::test]
async fn refused_tags_create_nothing() {
    let (dir, store) = store();
    let root = &project(dir.path());
    taggable(root);
    let mut host = writer(&store, root);
    let existing = unique("t");
    assert!(matches!(
        host.handle(create(&existing, None, None)).await,
        HostReply::Tag(_)
    ));
    let id = head(root).unwrap().to_string();
    let long = "m".repeat(16 * 1024 + 1);
    for (name, target, message, expected) in [
        ("", None, None, HostErrorCode::Invalid),
        ("-x", None, None, HostErrorCode::Invalid),
        ("HEAD", None, None, HostErrorCode::Invalid),
        ("a..b", None, None, HostErrorCode::Invalid),
        ("a b", None, None, HostErrorCode::Invalid),
        ("x/", None, None, HostErrorCode::Invalid),
        ("new", None, Some(" \n"), HostErrorCode::Invalid),
        ("new", None, Some(long.as_str()), HostErrorCode::Invalid),
        ("new", None, Some("a\0b"), HostErrorCode::Invalid),
        (existing.as_str(), None, None, HostErrorCode::Conflict),
        (existing.as_str(), None, Some("m"), HostErrorCode::Conflict),
        ("new", Some("missing"), None, HostErrorCode::NotFound),
        // Revision expressions and short ids are not evaluated.
        ("new", Some("HEAD~0"), None, HostErrorCode::NotFound),
        ("new", Some(&id[..12]), None, HostErrorCode::NotFound),
    ] {
        assert_eq!(
            code(host.handle(create(name, target, message)).await),
            Some(expected),
            "{name:?} {target:?} {message:?}"
        );
    }
    // Tags that would nest with an existing one, either way round.
    assert_eq!(
        code(
            host.handle(create(&format!("{existing}/child"), None, None))
                .await
        ),
        Some(HostErrorCode::Conflict)
    );
    let nested = format!("{}/leaf", unique("folder"));
    assert!(matches!(
        host.handle(create(&nested, None, None)).await,
        HostReply::Tag(_)
    ));
    let folder = nested.split('/').next().unwrap().to_string();
    assert_eq!(
        code(host.handle(create(&folder, None, Some("m"))).await),
        Some(HostErrorCode::Conflict)
    );
    let deeper = format!("{}{}", "a/".repeat(21), unique("t"));
    assert_eq!(
        code(host.handle(create(&deeper, None, None)).await),
        Some(HostErrorCode::Invalid)
    );
    assert_eq!(tags(&mut host).await.len(), 2);
}

#[tokio::test]
async fn tags_need_their_grants() {
    let (dir, store) = store();
    let root = &project(dir.path());
    taggable(root);
    let mut none = workspace_calls(&store, Some(root.into()), &[Capability::ReadWorkspace]);
    assert_eq!(
        code(none.handle(HostRequest::GitTags).await),
        Some(HostErrorCode::Denied)
    );
    let mut reader = workspace_calls(&store, Some(root.into()), &[Capability::GitRead]);
    assert!(tags(&mut reader).await.is_empty());
    assert_eq!(
        code(reader.handle(create(&unique("t"), None, None)).await),
        Some(HostErrorCode::Denied)
    );
    assert!(tags(&mut reader).await.is_empty());
}

#[tokio::test]
async fn protections_git_would_apply_refuse_a_tag() {
    for setup in [
        "hook",
        "no identity",
        "no commit",
        "signing",
        "forced annotation",
    ] {
        let (dir, store) = store();
        let root = &project(dir.path());
        let expected = match setup {
            "hook" => {
                taggable(root);
                std::fs::write(root.join(".git/hooks/reference-transaction"), "").unwrap();
                HostErrorCode::Unsupported
            }
            "no identity" => {
                repository(root, &[("a.txt", "one\n")]);
                HostErrorCode::Unsupported
            }
            "no commit" => {
                gix::init(root).unwrap();
                configure(root, "[user]\n\tname = n\n\temail = n@example.invalid\n");
                HostErrorCode::Invalid
            }
            "signing" => {
                taggable(root);
                configure(root, "[tag]\n\tgpgSign = true\n");
                HostErrorCode::Unsupported
            }
            _ => {
                // Yields to an explicit annotation, so nothing is refused.
                taggable(root);
                configure(root, "[tag]\n\tforceSignAnnotated = true\n");
                let mut host = writer(&store, root);
                for message in [Some("m"), None] {
                    let reply = host.handle(create(&unique("t"), None, message)).await;
                    assert!(matches!(reply, HostReply::Tag(_)), "{reply:?}");
                }
                continue;
            }
        };
        let mut host = writer(&store, root);
        assert_eq!(
            code(host.handle(create(&unique("t"), None, Some("m"))).await),
            Some(expected),
            "{setup}"
        );
        let count = || {
            gix::open(root)
                .unwrap()
                .references()
                .unwrap()
                .tags()
                .unwrap()
                .count()
        };
        assert_eq!(count(), 0, "{setup}");
        // Git needs no identity for a plain tag, but signs every tag with
        // tag.gpgSign.
        let plain = code(host.handle(create(&unique("t"), None, None)).await);
        match setup {
            "no identity" => {
                assert_eq!(plain, None);
                assert_eq!(count(), 1);
            }
            _ => {
                assert_eq!(plain, Some(expected), "{setup}");
                assert_eq!(count(), 0, "{setup}");
            }
        }
    }
}
