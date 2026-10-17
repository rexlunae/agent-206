//! Git host calls against repositories built with gitoxide, so the tests need
//! no `git` binary.
use super::*;
use a206_extensions::abi::{GitArea, GitChange, GitDiff};

/// A repository at `root` whose single commit holds `files`, with a matching
/// index and working tree.
fn repository(root: &std::path::Path, files: &[(&str, &str)]) -> (String, String) {
    let repo = gix::init(root).unwrap();
    let mut entries: Vec<_> = files
        .iter()
        .map(|(name, text)| gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Blob.into(),
            filename: (*name).into(),
            oid: repo.write_blob(text.as_bytes()).unwrap().detach(),
        })
        .collect();
    entries.sort_by(|a, b| a.filename.cmp(&b.filename));
    let tree = repo
        .write_object(gix::objs::Tree { entries })
        .unwrap()
        .detach();
    let (name, summary) = (unique("author"), unique("summary"));
    let signature = gix::actor::Signature {
        name: name.as_str().into(),
        email: "author@example.invalid".into(),
        time: gix::date::Time::new(1_000_000, 0),
    };
    let (mut first, mut second) = (Default::default(), Default::default());
    repo.commit_as(
        signature.to_ref(&mut first),
        signature.to_ref(&mut second),
        "HEAD",
        &summary,
        tree,
        Vec::<gix::ObjectId>::new(),
    )
    .unwrap();
    let mut index = repo.index_from_tree(&tree).unwrap();
    index.write(Default::default()).unwrap();
    for (name, text) in files {
        std::fs::write(root.join(name), text).unwrap();
    }
    (name, summary)
}
fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
}

#[tokio::test]
async fn git_calls_need_the_git_read_grant() {
    let (dir, store) = store();
    repository(dir.path(), &[("a.txt", "one\n")]);
    let mut host = workspace_calls(
        &store,
        Some(dir.path().into()),
        &[Capability::ReadWorkspace],
    );
    assert_eq!(
        code(host.handle(HostRequest::GitStatus).await),
        Some(HostErrorCode::Denied)
    );
}

#[tokio::test]
async fn status_log_and_diff_describe_the_repository() {
    let (dir, store) = store();
    let root = dir.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    let (author, summary) = repository(
        root.as_path(),
        &[("a.txt", "one\ntwo\n"), ("b.txt", "keep\n")],
    );
    std::fs::write(root.as_path().join("a.txt"), "one\nTWO\n").unwrap();
    std::fs::write(root.as_path().join("new.txt"), "fresh\n").unwrap();
    std::fs::remove_file(root.as_path().join("b.txt")).unwrap();
    let mut host = workspace_calls(&store, Some(root.clone()), &[Capability::GitRead]);

    let HostReply::GitStatus(status) = host.handle(HostRequest::GitStatus).await else {
        panic!("status failed");
    };
    assert!(status.branch.is_some());
    assert!(status.head.is_some());
    let entries: Vec<_> = status
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e.area, e.change))
        .collect();
    assert_eq!(
        entries,
        vec![
            ("a.txt", GitArea::Unstaged, GitChange::Modified),
            ("b.txt", GitArea::Unstaged, GitChange::Removed),
            ("new.txt", GitArea::Untracked, GitChange::Added),
        ]
    );

    let HostReply::Commits(commits) = host.handle(HostRequest::GitLog { max: 10 }).await else {
        panic!("log failed");
    };
    assert_eq!(commits.len(), 1);
    assert_eq!(
        (commits[0].author.as_str(), commits[0].summary.as_str()),
        (author.as_str(), summary.as_str())
    );
    assert_eq!(Some(&commits[0].id), status.head.as_ref());

    let diff = |path: &str, staged| HostRequest::GitDiff {
        path: path.into(),
        staged,
    };
    let HostReply::Diff(unstaged) = host.handle(diff("a.txt", false)).await else {
        panic!("diff failed");
    };
    assert_eq!(unstaged.text, "@@ -1,2 +1,2 @@\n one\n-two\n+TWO\n");
    assert_eq!(
        host.handle(diff("a.txt", true)).await,
        HostReply::Diff(GitDiff::default())
    );
    for path in ["../a.txt", ".git/config", "/etc/passwd"] {
        assert_eq!(
            code(host.handle(diff(path, false)).await),
            Some(HostErrorCode::Invalid),
            "{path}"
        );
    }
}

#[tokio::test]
async fn a_workspace_without_a_repository_is_reported() {
    let (dir, store) = store();
    // A repository above the workspace is not the workspace's.
    repository(dir.path(), &[("a.txt", "x\n")]);
    let inner = dir.path().join("inner");
    std::fs::create_dir(&inner).unwrap();
    let mut host = workspace_calls(&store, Some(inner), &[Capability::GitRead]);
    assert_eq!(
        code(host.handle(HostRequest::GitStatus).await),
        Some(HostErrorCode::NotFound)
    );
}

/// A repository's own configuration must not run programs: here a clean
/// filter that would create a marker file whenever content is compared.
#[cfg(unix)]
#[tokio::test]
async fn repository_programs_never_run() {
    let (dir, store) = store();
    let root = dir.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    repository(
        &root,
        &[
            ("a.txt", "same\n"),
            (".gitattributes", "* filter=probe diff=probe\n"),
        ],
    );
    let marker = dir.path().join("ran");
    let config = root.join(".git/config");
    let mut text = std::fs::read_to_string(&config).unwrap();
    let touch = format!("touch {}", marker.display());
    text.push_str(&format!(
        "[filter \"probe\"]\n\tclean = {touch}; cat\n\trequired = true\n[diff \"probe\"]\n\tcommand = {touch}\n\ttextconv = {touch}; cat\n[core]\n\tfsmonitor = {touch}\n\thooksPath = /nonexistent\n"
    ));
    std::fs::write(&config, text).unwrap();
    // Same size, new content: status has to compare content.
    std::fs::write(root.join("a.txt"), "diff\n").unwrap();
    let mut host = workspace_calls(&store, Some(root), &[Capability::GitRead]);
    let HostReply::GitStatus(status) = host.handle(HostRequest::GitStatus).await else {
        panic!("status failed");
    };
    assert!(status.entries.iter().any(|e| e.path == "a.txt"));
    let HostReply::Diff(diff) = host
        .handle(HostRequest::GitDiff {
            path: "a.txt".into(),
            staged: false,
        })
        .await
    else {
        panic!("diff failed");
    };
    assert!(diff.text.contains("+diff"));
    assert!(!marker.exists(), "a repository-configured program ran");
}
