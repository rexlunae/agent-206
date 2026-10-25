#![cfg(unix)]
//! A model pushes a branch or tag with `git_push` and fetches one with
//! `git_fetch`:
//! only after the user approves the call, through the vault broker, which
//! alone holds the forge token. The forge is a smart HTTP server on loopback
//! that stores what it receives in a bare repository with gitoxide's own
//! pack indexer, and serves fetches from it. Every identifier, file and key
//! is generated at runtime.
mod git_forge;
mod support;

use a206_core::*;
use git_forge::*;
use gix::ObjectId;
use serde_json::json;
use std::path::Path;
use support::*;

#[tokio::test(flavor = "multi_thread")]
async fn a_model_pushes_a_branch_only_after_approval_and_never_sees_the_token() {
    let mut rig = rig().await;

    // Denied: nothing is sent.
    let (record, _) = rig.push(json!({}), ApprovalDecision::Deny).await;
    assert_eq!(record.state, ToolCallState::Denied);
    assert!(rig.forge.seen.lock().unwrap().is_empty());

    // Approved: the forge gets the branch and every object it reaches.
    let first = gix::open(rig.workspace.path())
        .unwrap()
        .head_id()
        .unwrap()
        .detach();
    let (record, result) = rig.push(json!({}), ApprovalDecision::Approve).await;
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    let result: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
    assert_eq!(result["branch"], rig.branch.as_str());
    assert_eq!(result["commit"], first.to_string());
    assert_eq!(result["from"], serde_json::Value::Null);
    assert_eq!(result["commits"], 1);
    assert_eq!(rig.forge.branch(&rig.branch), Some(first));
    assert_eq!(tracking(rig.workspace.path(), &rig.branch), Some(first));
    {
        use base64::Engine as _;
        let basic = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD
                .encode(format!("{}:{}", rig.username, rig.token))
        );
        let seen = rig.forge.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen.iter().all(|(auth, _)| *auth == basic));
    }

    // A second commit fast-forwards the forge's branch.
    let second = commit(rig.workspace.path(), &[("src/main", &nonce())]);
    let (record, result) = rig
        .push(
            json!({"branch": rig.branch, "remote": "origin"}),
            ApprovalDecision::Approve,
        )
        .await;
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    let result: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
    assert_eq!(result["from"], first.to_string());
    assert_eq!(result["commits"], 1);
    // The commit, the root tree, `src` and the changed file.
    assert_eq!(result["objects"], 4);
    assert_eq!(rig.forge.branch(&rig.branch), Some(second));

    // Nothing new: no pack is sent.
    let posts = rig.forge.posts();
    let (record, result) = rig.push(json!({}), ApprovalDecision::Approve).await;
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    assert!(result.unwrap().contains("\"up_to_date\":true"));
    assert_eq!(rig.forge.posts(), posts);

    // The token never reached a profile file.
    let mut pending = vec![rig._state.path().to_owned()];
    while let Some(path) = pending.pop() {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            } else if entry.file_type().unwrap().is_file() {
                let bytes = std::fs::read(entry.path()).unwrap();
                let needle = rig.token.as_bytes();
                assert!(
                    !bytes.windows(needle.len()).any(|w| w == needle),
                    "{} holds the token",
                    entry.path().display()
                );
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pushes_that_would_lose_commits_or_that_the_forge_refuses_change_nothing() {
    let mut rig = rig().await;
    let (record, _) = rig.push(json!({}), ApprovalDecision::Approve).await;
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    let pushed = rig.forge.branch(&rig.branch).unwrap();

    // Someone else moved the forge's branch to a commit this repository
    // lacks: the push is refused before any pack is sent.
    let elsewhere = {
        let repo = gix::open(&rig.forge.bare).unwrap();
        let tree = repo
            .find_commit(pushed)
            .unwrap()
            .tree_id()
            .unwrap()
            .detach();
        let signature = gix::actor::Signature {
            name: "o".into(),
            email: "o@example.invalid".into(),
            time: gix::date::Time::new(2_000_000, 0),
        };
        let (mut a, mut b) = Default::default();
        repo.commit_as(
            signature.to_ref(&mut a),
            signature.to_ref(&mut b),
            format!("refs/heads/{}", rig.branch),
            nonce(),
            tree,
            [pushed],
        )
        .unwrap()
        .detach()
    };
    commit(rig.workspace.path(), &[("readme", &nonce())]);
    let posts = rig.forge.posts();
    let (record, _) = rig.push(json!({}), ApprovalDecision::Approve).await;
    assert_eq!(record.state, ToolCallState::Failed);
    assert_eq!(error_kind(&record), ErrorKind::Conflict);
    assert_eq!(rig.forge.posts(), posts);
    assert_eq!(rig.forge.branch(&rig.branch), Some(elsewhere));

    // A branch the forge protects is refused with its reason, and the
    // remote-tracking branch stays where it was.
    let reason = format!("protected {}", nonce());
    *rig.forge.refuse.lock().unwrap() = Some(reason.clone());
    let topic = format!("t-{}", nonce());
    {
        let repo = gix::open(rig.workspace.path()).unwrap();
        let head = repo.head_id().unwrap().detach();
        repo.reference(
            format!("refs/heads/{topic}"),
            head,
            gix::refs::transaction::PreviousValue::MustNotExist,
            "test",
        )
        .unwrap();
    }
    let (record, _) = rig
        .push(json!({"branch": topic}), ApprovalDecision::Approve)
        .await;
    assert_eq!(record.state, ToolCallState::Failed);
    assert_eq!(error_kind(&record), ErrorKind::Conflict);
    let cause = record.error.as_ref().unwrap().source.as_ref().unwrap();
    assert!(cause.message.contains(&reason), "{:?}", record.error);
    assert_eq!(rig.forge.branch(&topic), None);
    assert_eq!(tracking(rig.workspace.path(), &topic), None);

    // A remote on a forge with no git credential set is refused before the
    // broker is asked for anything.
    let config = rig.workspace.path().join(".git/config");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("[remote \"mirror\"]\n\turl = https://example.invalid/team/repo.git\n");
    std::fs::write(&config, text).unwrap();
    let seen = rig.forge.seen.lock().unwrap().len();
    let (record, _) = rig
        .push(json!({"remote": "mirror"}), ApprovalDecision::Approve)
        .await;
    assert_eq!(record.state, ToolCallState::Failed);
    assert_eq!(error_kind(&record), ErrorKind::PermissionDenied);
    assert_eq!(rig.forge.seen.lock().unwrap().len(), seen);
}

/// Tag the current commit as `name`, annotated when `message` is given; the
/// object the tag points at.
fn tag(root: &Path, name: &str, message: Option<&str>) -> ObjectId {
    let repo = gix::open(root).unwrap();
    let head = repo.head_id().unwrap().detach();
    let target = match message {
        None => head,
        Some(message) => repo
            .write_object(gix::objs::Tag {
                target: head,
                target_kind: gix::object::Kind::Commit,
                name: name.into(),
                tagger: None,
                message: message.into(),
                signature: None,
            })
            .unwrap()
            .detach(),
    };
    repo.reference(
        format!("refs/tags/{name}"),
        target,
        gix::refs::transaction::PreviousValue::Any,
        "test",
    )
    .unwrap();
    target
}

#[tokio::test(flavor = "multi_thread")]
async fn a_model_pushes_a_tag_the_forge_lacks_and_never_replaces_one() {
    let mut rig = rig().await;
    let head = gix::open(rig.workspace.path())
        .unwrap()
        .head_id()
        .unwrap()
        .detach();

    // An annotated tag goes with the commit it names, before its branch.
    let annotated = format!("v{}", nonce());
    let object = tag(rig.workspace.path(), &annotated, Some(&nonce()));
    let (record, text) = rig
        .push(json!({"tag": annotated}), ApprovalDecision::Approve)
        .await;
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    let pushed = result(text);
    assert_eq!(pushed["tag"], annotated.as_str());
    assert_eq!(pushed["annotated"], true);
    assert_eq!(pushed["commit"], head.to_string());
    assert_eq!(pushed["commits"], 1);
    assert_eq!(pushed.get("tracking_branch"), None);
    assert_eq!(rig.forge.tag(&annotated), Some(object));
    assert_eq!(rig.forge.branch(&rig.branch), None);

    // A plain tag of a commit the forge has sends no commits.
    let plain = format!("p{}", nonce());
    tag(rig.workspace.path(), &plain, None);
    let (record, text) = rig
        .push(json!({"tag": plain}), ApprovalDecision::Approve)
        .await;
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    let pushed = result(text);
    assert_eq!(
        (pushed["annotated"].clone(), pushed["commits"].clone()),
        (false.into(), 0.into())
    );
    assert_eq!(rig.forge.tag(&plain), Some(head));

    // Pushing it again sends nothing.
    let posts = rig.forge.posts();
    let (record, text) = rig
        .push(json!({"tag": annotated}), ApprovalDecision::Approve)
        .await;
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    assert_eq!(result(text)["up_to_date"], true);
    assert_eq!(rig.forge.posts(), posts);

    // A local tag moved to another commit never replaces the forge's.
    commit(rig.workspace.path(), &[("readme", &nonce())]);
    tag(rig.workspace.path(), &plain, None);
    let (record, _) = rig
        .push(json!({"tag": plain}), ApprovalDecision::Approve)
        .await;
    assert_eq!(record.state, ToolCallState::Failed);
    assert_eq!(error_kind(&record), ErrorKind::Conflict);
    assert_eq!(rig.forge.posts(), posts);
    assert_eq!(rig.forge.tag(&plain), Some(head));

    // A branch and a tag at once is refused.
    let seen = rig.forge.seen.lock().unwrap().len();
    let (record, _) = rig
        .push(
            json!({"tag": plain, "branch": rig.branch}),
            ApprovalDecision::Approve,
        )
        .await;
    assert_eq!(error_kind(&record), ErrorKind::InvalidRequest);
    assert_eq!(rig.forge.seen.lock().unwrap().len(), seen);
}

/// The forge's tag `name` at `target`, annotated when `annotated`; the
/// object the tag points at.
fn forge_tag(forge: &Forge, name: &str, target: ObjectId, annotated: bool) -> ObjectId {
    let repo = gix::open(&forge.bare).unwrap();
    let object = match annotated {
        false => target,
        true => repo
            .write_object(gix::objs::Tag {
                target,
                target_kind: gix::object::Kind::Commit,
                name: name.into(),
                tagger: None,
                message: nonce().into(),
                signature: None,
            })
            .unwrap()
            .detach(),
    };
    repo.reference(
        format!("refs/tags/{name}"),
        object,
        gix::refs::transaction::PreviousValue::Any,
        "test",
    )
    .unwrap();
    object
}
fn local_tag(root: &Path, name: &str) -> Option<ObjectId> {
    gix::open(root)
        .unwrap()
        .try_find_reference(format!("refs/tags/{name}").as_str())
        .unwrap()
        .map(|mut r| r.follow_to_object().unwrap().detach())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_model_fetches_a_tag_the_project_lacks_and_never_replaces_one() {
    let mut rig = rig().await;
    let (record, _) = rig.push(json!({}), ApprovalDecision::Approve).await;
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    let pushed = rig.forge.branch(&rig.branch).unwrap();
    // A release tagged on the forge, on a commit this project lacks.
    let released = rig.forge.commit(&format!("r-{}", nonce()), &[pushed]);
    let release = format!("v{}", nonce());
    let object = forge_tag(&rig.forge, &release, released, true);

    let (record, text) = rig
        .fetch(json!({"tag": release}), ApprovalDecision::Approve)
        .await;
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    let fetched = result(text);
    assert_eq!(fetched["tag"], release.as_str());
    assert_eq!(fetched["annotated"], true);
    assert_eq!(fetched["commit"], released.to_string());
    assert_eq!(fetched["up_to_date"], false);
    assert_eq!(fetched.get("tracking_branch"), None);
    // The tag object, the commit, its root tree, the new folder and file.
    assert_eq!(fetched["objects"], 5);
    assert_eq!(local_tag(rig.workspace.path(), &release), Some(object));
    let repo = gix::open(rig.workspace.path()).unwrap();
    repo.find_commit(released)
        .unwrap()
        .tree()
        .unwrap()
        .traverse()
        .breadthfirst(&mut gix::traverse::tree::Recorder::default())
        .unwrap();
    // Branches stay where they were.
    assert_eq!(tracking(rig.workspace.path(), &rig.branch), Some(pushed));

    // Again: nothing is downloaded.
    let uploads = rig.forge.count("git-upload-pack");
    let (record, text) = rig
        .fetch(json!({"tag": release}), ApprovalDecision::Approve)
        .await;
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    assert_eq!(result(text)["up_to_date"], true);
    assert_eq!(rig.forge.count("git-upload-pack"), uploads);

    // A plain tag of a commit the project has needs no pack either.
    let plain = format!("p{}", nonce());
    forge_tag(&rig.forge, &plain, pushed, false);
    let (record, text) = rig
        .fetch(json!({"tag": plain}), ApprovalDecision::Approve)
        .await;
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    let fetched = result(text);
    assert_eq!(
        (fetched["annotated"].clone(), fetched["objects"].clone()),
        (false.into(), 0.into())
    );
    assert_eq!(local_tag(rig.workspace.path(), &plain), Some(pushed));
    assert_eq!(rig.forge.count("git-upload-pack"), uploads);

    // A local tag of the same name at another object is never replaced,
    // and nothing is downloaded for it.
    let moved = forge_tag(&rig.forge, &plain, released, true);
    let (record, _) = rig
        .fetch(json!({"tag": plain}), ApprovalDecision::Approve)
        .await;
    assert_eq!(record.state, ToolCallState::Failed);
    assert_eq!(error_kind(&record), ErrorKind::Conflict);
    assert_ne!(local_tag(rig.workspace.path(), &plain), Some(moved));
    assert_eq!(rig.forge.count("git-upload-pack"), uploads);

    // A tag the forge lacks is not found; a branch and a tag at once is
    // refused before the forge is asked.
    let (record, _) = rig
        .fetch(
            json!({"tag": format!("t-{}", nonce())}),
            ApprovalDecision::Approve,
        )
        .await;
    assert_eq!(error_kind(&record), ErrorKind::NotFound);
    let seen = rig.forge.seen.lock().unwrap().len();
    let (record, _) = rig
        .fetch(
            json!({"tag": plain, "branch": rig.branch}),
            ApprovalDecision::Approve,
        )
        .await;
    assert_eq!(error_kind(&record), ErrorKind::InvalidRequest);
    assert_eq!(rig.forge.seen.lock().unwrap().len(), seen);
}

fn result(text: Option<String>) -> serde_json::Value {
    serde_json::from_str(&text.unwrap()).unwrap()
}
fn packs(root: &Path) -> usize {
    std::fs::read_dir(root.join(".git/objects/pack"))
        .map(|entries| entries.count())
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_model_fetches_a_branch_into_its_remote_tracking_branch_only_after_approval() {
    let mut rig = rig().await;
    let (record, _) = rig.push(json!({}), ApprovalDecision::Approve).await;
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    let pushed = rig.forge.branch(&rig.branch).unwrap();
    let elsewhere = rig.forge.commit(&rig.branch, &[pushed]);

    // Denied: the forge is not asked for anything.
    let seen = rig.forge.seen.lock().unwrap().len();
    let (record, _) = rig.fetch(json!({}), ApprovalDecision::Deny).await;
    assert_eq!(record.state, ToolCallState::Denied);
    assert_eq!(rig.forge.seen.lock().unwrap().len(), seen);

    // Approved: the remote-tracking branch moves to the forge's commit with
    // everything it reaches; the local branch stays where it was.
    let local = gix::open(rig.workspace.path())
        .unwrap()
        .head_id()
        .unwrap()
        .detach();
    let (record, text) = rig.fetch(json!({}), ApprovalDecision::Approve).await;
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    let fetched = result(text);
    assert_eq!(fetched["tracking_branch"], format!("origin/{}", rig.branch));
    assert_eq!(fetched["from"], pushed.to_string());
    assert_eq!(fetched["commit"], elsewhere.to_string());
    assert_eq!(fetched["forced_update"], false);
    // The commit, its root tree, the new folder and the new file.
    assert_eq!(fetched["objects"], 4);
    assert_eq!(tracking(rig.workspace.path(), &rig.branch), Some(elsewhere));
    let repo = gix::open(rig.workspace.path()).unwrap();
    assert_eq!(repo.head_id().unwrap().detach(), local);
    repo.find_commit(elsewhere)
        .unwrap()
        .tree()
        .unwrap()
        .traverse()
        .breadthfirst(&mut gix::traverse::tree::Recorder::default())
        .unwrap();
    // Only the remote's own pushes reached its push service.
    {
        use base64::Engine as _;
        let basic = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD
                .encode(format!("{}:{}", rig.username, rig.token))
        );
        assert!(
            rig.forge
                .seen
                .lock()
                .unwrap()
                .iter()
                .all(|(auth, _)| *auth == basic)
        );
    }
    assert_eq!(rig.forge.count("git-upload-pack"), 1);

    // Nothing new: no pack is asked for.
    let (record, text) = rig
        .fetch(
            json!({"branch": rig.branch, "remote": "origin"}),
            ApprovalDecision::Approve,
        )
        .await;
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    assert_eq!(result(text)["up_to_date"], true);
    assert_eq!(rig.forge.count("git-upload-pack"), 1);

    // A branch the remote lacks is not found.
    let (record, _) = rig
        .fetch(
            json!({"branch": format!("b-{}", nonce())}),
            ApprovalDecision::Approve,
        )
        .await;
    assert_eq!(record.state, ToolCallState::Failed);
    assert_eq!(error_kind(&record), ErrorKind::NotFound);
}

#[tokio::test(flavor = "multi_thread")]
async fn fetches_follow_the_refspec_and_store_nothing_incomplete() {
    let mut rig = rig().await;
    let (record, _) = rig.push(json!({}), ApprovalDecision::Approve).await;
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    let pushed = rig.forge.branch(&rig.branch).unwrap();

    // The forge's branch was replaced with unrelated history: `+` in the
    // refspec allows the forced update, which the result reports.
    let replaced = rig.forge.commit(&rig.branch, &[]);
    let (record, text) = rig.fetch(json!({}), ApprovalDecision::Approve).await;
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    assert_eq!(result(text)["forced_update"], true);
    assert_eq!(tracking(rig.workspace.path(), &rig.branch), Some(replaced));

    // Without `+`, a branch that no longer contains the remote-tracking one
    // is refused and nothing moves.
    let config = rig.workspace.path().join(".git/config");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, text.replace("fetch = +refs/", "fetch = refs/")).unwrap();
    let again = rig.forge.commit(&rig.branch, &[pushed]);
    let before = packs(rig.workspace.path());
    let (record, _) = rig.fetch(json!({}), ApprovalDecision::Approve).await;
    assert_eq!(record.state, ToolCallState::Failed);
    assert_eq!(error_kind(&record), ErrorKind::Conflict);
    assert_eq!(tracking(rig.workspace.path(), &rig.branch), Some(replaced));
    // The refused pack was removed again.
    assert_eq!(packs(rig.workspace.path()), before);

    // A pack missing an object the commit reaches is refused and removed.
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, text.replace("fetch = refs/", "fetch = +refs/")).unwrap();
    rig.forge
        .incomplete
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (record, _) = rig.fetch(json!({}), ApprovalDecision::Approve).await;
    assert_eq!(record.state, ToolCallState::Failed);
    assert_eq!(error_kind(&record), ErrorKind::Protocol);
    assert_eq!(tracking(rig.workspace.path(), &rig.branch), Some(replaced));
    assert_eq!(packs(rig.workspace.path()), before);
    rig.forge
        .incomplete
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let (record, _) = rig.fetch(json!({}), ApprovalDecision::Approve).await;
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    assert_eq!(tracking(rig.workspace.path(), &rig.branch), Some(again));

    // A remote whose branches are mapped differently is refused before the
    // forge is asked for anything.
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        text.replace(
            "fetch = +refs/heads/*:refs/remotes/origin/*",
            "fetch = +refs/heads/*:refs/heads/*",
        ),
    )
    .unwrap();
    let seen = rig.forge.seen.lock().unwrap().len();
    let (record, _) = rig.fetch(json!({}), ApprovalDecision::Approve).await;
    assert_eq!(record.state, ToolCallState::Failed);
    assert_eq!(error_kind(&record), ErrorKind::InvalidRequest);
    assert_eq!(rig.forge.seen.lock().unwrap().len(), seen);

    // So is the usual mapping beside another one, which git would also
    // update.
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        text.replace(
            "fetch = +refs/heads/*:refs/heads/*",
            "fetch = +refs/heads/*:refs/remotes/origin/*\n\tfetch = +refs/tags/*:refs/tags/*",
        ),
    )
    .unwrap();
    let (record, _) = rig.fetch(json!({}), ApprovalDecision::Approve).await;
    assert_eq!(record.state, ToolCallState::Failed);
    assert_eq!(error_kind(&record), ErrorKind::InvalidRequest);
    assert_eq!(rig.forge.seen.lock().unwrap().len(), seen);
}
