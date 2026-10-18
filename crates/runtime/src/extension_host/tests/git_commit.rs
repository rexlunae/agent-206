//! The git commit host call, against repositories built with gitoxide. The
//! user's own git configuration is never read here (see `calls_for`).
use super::git::{repository, unique};
use super::*;
use crate::extension_host::git_commit as host;
use a206_extensions::abi::{GitArea, GitChange};
use std::path::Path;

/// A repository holding `files` whose own configuration names a committer.
fn committable(root: &Path, files: &[(&str, &str)]) -> (String, String) {
    repository(root, files);
    let (name, email) = (unique("name"), format!("{}@example.invalid", unique("e")));
    configure(
        root,
        &format!("[user]\n\tname = {name}\n\temail = {email}\n"),
    );
    (name, email)
}
/// The repository lives beside the gateway's own data, never around it.
pub(super) fn project(dir: &Path) -> std::path::PathBuf {
    let root = dir.join("repo");
    std::fs::create_dir(&root).unwrap();
    root
}
pub(super) fn configure(root: &Path, text: &str) {
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(root.join(".git/config"))
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
}
fn commit(message: &str, paths: &[&str]) -> HostRequest {
    HostRequest::GitCommit {
        message: message.into(),
        paths: paths.iter().map(|path| (*path).into()).collect(),
    }
}
fn commit_staged(message: &str) -> HostRequest {
    HostRequest::GitCommitStaged {
        message: message.into(),
    }
}
async fn stage(host: &mut HostCalls, paths: &[&str]) {
    let reply = host
        .handle(HostRequest::GitStage {
            paths: paths.iter().map(|path| (*path).into()).collect(),
            unstage: false,
        })
        .await;
    assert!(matches!(reply, HostReply::Done), "{reply:?}");
}
pub(super) fn writer(store: &Arc<Mutex<Store>>, root: &Path) -> HostCalls {
    workspace_calls(
        store,
        Some(root.into()),
        &[Capability::GitRead, Capability::GitWrite],
    )
}
pub(super) fn head(root: &Path) -> Option<gix::ObjectId> {
    gix::open(root).unwrap().head_id().ok().map(gix::Id::detach)
}
/// The blob text at `path` in `HEAD`, if any.
fn committed(root: &Path, path: &str) -> Option<String> {
    let repo = gix::open(root).unwrap();
    let tree = repo.head_tree().ok()?;
    let entry = tree.lookup_entry_by_path(path).unwrap()?;
    let blob = repo.find_object(entry.object_id()).unwrap();
    Some(String::from_utf8(blob.detach().data).unwrap())
}
pub(super) async fn changes(host: &mut HostCalls) -> Vec<(String, GitArea, GitChange)> {
    let HostReply::GitStatus(status) = host.handle(HostRequest::GitStatus).await else {
        panic!("status failed");
    };
    status
        .entries
        .into_iter()
        .map(|e| (e.path, e.area, e.change))
        .collect()
}

#[tokio::test]
async fn only_the_named_paths_are_committed_and_the_index_follows() {
    let (dir, store) = store();
    let root = &project(dir.path());
    let (name, _) = committable(
        root,
        &[
            ("a.txt", "one\n"),
            ("b.txt", "two\n"),
            ("gone.txt", "old\n"),
        ],
    );
    let (a, b, c) = (unique("a"), unique("b"), unique("c"));
    std::fs::write(root.join("a.txt"), &a).unwrap();
    std::fs::write(root.join("b.txt"), &b).unwrap();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/c.txt"), &c).unwrap();
    std::fs::remove_file(root.join("gone.txt")).unwrap();
    let before = head(root).unwrap();
    let summary = unique("summary");
    let mut host = writer(&store, root);

    let reply = host
        .handle(commit(
            &format!("{summary}\n\nbody"),
            &["a.txt", "sub//c.txt", "gone.txt", "a.txt"],
        ))
        .await;
    let HostReply::Committed(made) = reply else {
        panic!("commit failed: {reply:?}");
    };
    assert_eq!(made.summary, summary);
    assert_eq!(made.author, name);
    let repo = gix::open(root).unwrap();
    let id = repo.head_id().unwrap().detach();
    assert_eq!(made.id, id.to_string());
    let made = repo.find_commit(id).unwrap();
    assert_eq!(
        made.parent_ids().map(gix::Id::detach).collect::<Vec<_>>(),
        [before]
    );
    assert_eq!(committed(root, "a.txt"), Some(a));
    assert_eq!(committed(root, "sub/c.txt"), Some(c));
    assert_eq!(committed(root, "b.txt").as_deref(), Some("two\n"));
    assert_eq!(committed(root, "gone.txt"), None);
    // The committed paths are clean; the one left out is still unstaged.
    assert_eq!(
        changes(&mut host).await,
        [("b.txt".to_string(), GitArea::Unstaged, GitChange::Modified)]
    );
}

#[tokio::test]
async fn the_first_commit_of_a_new_repository_is_made() {
    let (dir, store) = store();
    let root = &project(dir.path());
    gix::init(root).unwrap();
    configure(root, "[user]\n\tname = n\n\temail = n@example.invalid\n");
    let text = unique("text");
    std::fs::write(root.join("first.txt"), &text).unwrap();
    let mut host = writer(&store, root);
    let reply = host.handle(commit(&unique("first"), &["first.txt"])).await;
    assert!(matches!(reply, HostReply::Committed(_)), "{reply:?}");
    assert_eq!(committed(root, "first.txt"), Some(text));
    assert!(changes(&mut host).await.is_empty());
}

#[tokio::test]
async fn committing_needs_the_git_write_grant() {
    let (dir, store) = store();
    let root = &project(dir.path());
    committable(root, &[("a.txt", "one\n")]);
    std::fs::write(root.join("a.txt"), unique("a")).unwrap();
    let before = head(root);
    let mut reader = workspace_calls(
        &store,
        Some(root.into()),
        &[Capability::GitRead, Capability::WriteWorkspace],
    );
    assert_eq!(
        code(reader.handle(commit("m", &["a.txt"])).await),
        Some(HostErrorCode::Denied)
    );
    assert_eq!(head(root), before);
}

#[tokio::test]
async fn protections_git_would_apply_refuse_the_commit() {
    for (setup, expected) in [
        ("no identity", HostErrorCode::Unsupported),
        ("signing", HostErrorCode::Unsupported),
        ("hook", HostErrorCode::Unsupported),
        ("hooks path", HostErrorCode::Unsupported),
        ("filter", HostErrorCode::Unsupported),
        ("line endings", HostErrorCode::Unsupported),
        ("ignored", HostErrorCode::Invalid),
        ("locked", HostErrorCode::Conflict),
    ] {
        let (dir, store) = store();
        let root = &project(dir.path());
        if setup == "no identity" {
            repository(root, &[("a.txt", "one\n")]);
        } else {
            committable(root, &[("a.txt", "one\n")]);
        }
        let mut path = "a.txt";
        let mut text = unique("a");
        match setup {
            "signing" => configure(root, "[commit]\n\tgpgSign = true\n"),
            "hook" => std::fs::write(root.join(".git/hooks/pre-commit"), "").unwrap(),
            "hooks path" => {
                std::fs::create_dir(root.join("hooks")).unwrap();
                std::fs::write(root.join("hooks/commit-msg"), "").unwrap();
                configure(root, "[core]\n\thooksPath = hooks\n");
            }
            "filter" => std::fs::write(root.join(".gitattributes"), "*.txt filter=lfs\n").unwrap(),
            "line endings" => {
                std::fs::write(root.join(".gitattributes"), "*.txt text\n").unwrap();
                text = format!("{text}\r\n");
            }
            "ignored" => {
                std::fs::write(root.join(".gitignore"), "*.env\n").unwrap();
                path = "secret.env";
            }
            "locked" => std::fs::write(root.join(".git/index.lock"), "").unwrap(),
            _ => {}
        }
        std::fs::write(root.join(path), &text).unwrap();
        let before = head(root);
        let mut host = writer(&store, root);
        assert_eq!(
            code(host.handle(commit("m", &[path])).await),
            Some(expected),
            "{setup}"
        );
        assert_eq!(head(root), before, "{setup}");
    }
}

#[tokio::test]
async fn invalid_requests_change_nothing() {
    let (dir, store) = store();
    let root = &project(dir.path());
    committable(root, &[("a.txt", "one\n")]);
    let before = head(root);
    let mut host = writer(&store, root);
    let many: Vec<String> = (0..=host::MAX_COMMIT_PATHS)
        .map(|n| format!("f{n}"))
        .collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    for (message, paths, expected) in [
        ("m", vec!["../outside"], HostErrorCode::Invalid),
        ("m", vec![".git/config"], HostErrorCode::Invalid),
        ("m", vec!["/etc/passwd"], HostErrorCode::Invalid),
        ("m", vec![], HostErrorCode::Invalid),
        ("m", many, HostErrorCode::Invalid),
        ("  \n", vec!["a.txt"], HostErrorCode::Invalid),
        ("m", vec!["missing.txt"], HostErrorCode::NotFound),
        // Unchanged: nothing to commit.
        ("m", vec!["a.txt"], HostErrorCode::Invalid),
    ] {
        assert_eq!(
            code(host.handle(commit(message, &paths)).await),
            Some(expected),
            "{paths:?}"
        );
    }
    assert_eq!(head(root), before);
}

#[test]
fn an_interrupted_commit_stops_before_committing() {
    let dir = tempfile::tempdir().unwrap();
    let root = &project(dir.path());
    committable(root, &[("a.txt", "one\n")]);
    std::fs::write(root.join("a.txt"), unique("a")).unwrap();
    let before = head(root);
    let stopped = std::sync::atomic::AtomicBool::new(true);
    let refused = host::commit(
        root.into(),
        "m",
        host::Selection::Paths(&["a.txt".into()]),
        &host::UserGit::default(),
        &stopped,
    )
    .unwrap_err();
    assert_eq!(refused.code, HostErrorCode::Budget);
    assert_eq!(head(root), before);
}

#[tokio::test]
async fn a_write_finishing_after_the_deadline_reports_its_outcome() {
    let wait = |interrupt: Arc<std::sync::atomic::AtomicBool>| {
        while !interrupt.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::yield_now();
        }
    };
    let made = super::super::git_write_call_with_timeout(
        move |interrupt| {
            wait(interrupt);
            Ok(HostReply::Done)
        },
        std::time::Duration::ZERO,
    )
    .await;
    assert_eq!(made, Ok(HostReply::Done));
}

#[cfg(unix)]
#[tokio::test]
async fn a_file_replaced_by_a_directory_is_not_committed_as_a_removal() {
    let (dir, store) = store();
    let root = &project(dir.path());
    committable(root, &[("a", "one\n")]);
    std::fs::remove_file(root.join("a")).unwrap();
    std::fs::create_dir(root.join("a")).unwrap();
    std::fs::write(root.join("a/x"), unique("x")).unwrap();
    let before = head(root);
    let mut host = writer(&store, root);
    assert_eq!(
        code(host.handle(commit("m", &["a"])).await),
        Some(HostErrorCode::Unsupported)
    );
    assert_eq!(head(root), before);
    assert_eq!(committed(root, "a").as_deref(), Some("one\n"));
}

#[tokio::test]
async fn modes_and_links_are_recorded_without_following_links() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, store) = store();
    let root = dir.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    committable(&root, &[("run.sh", "true\n")]);
    std::fs::write(dir.path().join("outside.txt"), "secret").unwrap();
    std::fs::set_permissions(root.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("run.sh", root.join("link")).unwrap();
    std::os::unix::fs::symlink(dir.path(), root.join("out")).unwrap();
    let mut host = writer(&store, &root);
    assert_eq!(
        code(host.handle(commit("m", &["out/outside.txt"])).await),
        Some(HostErrorCode::Invalid)
    );
    std::fs::remove_file(root.join("out")).unwrap();
    let reply = host.handle(commit("m", &["run.sh", "link"])).await;
    assert!(matches!(reply, HostReply::Committed(_)), "{reply:?}");
    let repo = gix::open(&root).unwrap();
    let tree = repo.head_tree().unwrap();
    let kind = |path| {
        tree.lookup_entry_by_path(path)
            .unwrap()
            .unwrap()
            .mode()
            .kind()
    };
    assert_eq!(kind("run.sh"), gix::objs::tree::EntryKind::BlobExecutable);
    assert_eq!(kind("link"), gix::objs::tree::EntryKind::Link);
    assert_eq!(committed(&root, "link").as_deref(), Some("run.sh"));
    assert_eq!(changes(&mut host).await, []);
}

/// Refusals that depend on configuration git reads beyond the repository's
/// own file: includes, conditional includes and the user's settings.
#[test]
fn settings_git_would_follow_are_honoured() {
    let user = |text: &str, excludes: Option<std::path::PathBuf>| {
        host::UserGit::given(text.parse().unwrap(), excludes)
    };
    for case in [
        "include",
        "conditional",
        "user excludes",
        "repository excludes",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = &project(dir.path());
        committable(root, &[("a.txt", "one\n")]);
        let signing = dir.path().join("signing.inc");
        std::fs::write(&signing, "[commit]\n\tgpgSign = true\n").unwrap();
        let ignore = dir.path().join("ignore");
        std::fs::write(&ignore, "*.log\n").unwrap();
        let (settings, path, expected) = match case {
            "include" => {
                configure(
                    root,
                    &format!("[include]\n\tpath = {}\n", signing.display()),
                );
                (user("", None), "a.txt", HostErrorCode::Unsupported)
            }
            "conditional" => (
                user(
                    &format!(
                        "[includeIf \"gitdir:{}/\"]\n\tpath = {}\n",
                        dir.path().display(),
                        signing.display()
                    ),
                    None,
                ),
                "a.txt",
                HostErrorCode::Unsupported,
            ),
            "user excludes" => (
                user("", Some(ignore.clone())),
                "new.log",
                HostErrorCode::Invalid,
            ),
            _ => {
                // The repository's own excludes file replaces the user's.
                let own = dir.path().join("own-ignore");
                std::fs::write(&own, "*.tmp\n").unwrap();
                configure(
                    root,
                    &format!("[core]\n\texcludesFile = {}\n", own.display()),
                );
                (
                    user("", Some(ignore.clone())),
                    "new.tmp",
                    HostErrorCode::Invalid,
                )
            }
        };
        std::fs::write(root.join(path), unique("text")).unwrap();
        let before = head(root);
        let stop = std::sync::atomic::AtomicBool::new(false);
        let refused = host::commit(
            root.clone(),
            "m",
            host::Selection::Paths(&[path.into()]),
            &settings,
            &stop,
        )
        .expect_err(case);
        assert_eq!(refused.code, expected, "{case}: {}", refused.message);
        assert_eq!(head(root), before, "{case}");
    }
    // Outside the repository's own excludes file, the user's patterns no
    // longer apply, so the same name commits.
    let dir = tempfile::tempdir().unwrap();
    let root = &project(dir.path());
    committable(root, &[("a.txt", "one\n")]);
    let ignore = dir.path().join("ignore");
    std::fs::write(&ignore, "*.log\n").unwrap();
    let own = dir.path().join("own-ignore");
    std::fs::write(&own, "*.tmp\n").unwrap();
    configure(
        root,
        &format!("[core]\n\texcludesFile = {}\n", own.display()),
    );
    std::fs::write(root.join("new.log"), unique("text")).unwrap();
    let stop = std::sync::atomic::AtomicBool::new(false);
    let made = host::commit(
        root.clone(),
        "m",
        host::Selection::Paths(&["new.log".into()]),
        &host::UserGit::given(Default::default(), Some(ignore)),
        &stop,
    );
    assert!(matches!(made, Ok(HostReply::Committed(_))), "{made:?}");
}

#[tokio::test]
async fn a_commit_is_refused_without_time_to_report_it() {
    let (dir, store) = store();
    let root = &project(dir.path());
    committable(root, &[("a.txt", "one\n")]);
    std::fs::write(root.join("a.txt"), unique("a")).unwrap();
    let before = head(root);
    let mut host = writer(&store, root);
    host.deadline = tokio::time::Instant::now() + GIT_WRITE_RESERVE;
    assert_eq!(
        code(host.handle(commit("m", &["a.txt"])).await),
        Some(HostErrorCode::Budget)
    );
    assert_eq!(head(root), before);
}

#[tokio::test]
async fn committing_what_is_staged_records_the_index_and_leaves_it() {
    let (dir, store) = store();
    let root = &project(dir.path());
    let (name, _) = committable(
        root,
        &[
            ("a.txt", "one\n"),
            ("b.txt", "two\n"),
            ("gone.txt", "old\n"),
        ],
    );
    let (staged, later, b, new) = (
        unique("staged"),
        unique("later"),
        unique("b"),
        unique("new"),
    );
    let mut host = writer(&store, root);
    std::fs::write(root.join("a.txt"), &staged).unwrap();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/new.txt"), &new).unwrap();
    std::fs::remove_file(root.join("gone.txt")).unwrap();
    stage(&mut host, &["a.txt", "sub/new.txt", "gone.txt"]).await;
    // Changed after staging, and never staged: neither is committed.
    std::fs::write(root.join("a.txt"), &later).unwrap();
    std::fs::write(root.join("b.txt"), &b).unwrap();
    let before = head(root).unwrap();
    let index = std::fs::read(root.join(".git/index")).unwrap();
    let summary = unique("summary");

    let reply = host.handle(commit_staged(&summary)).await;
    let HostReply::Committed(made) = reply else {
        panic!("commit failed: {reply:?}");
    };
    assert_eq!(
        (made.summary.as_str(), made.author.as_str()),
        (summary.as_str(), name.as_str())
    );
    let repo = gix::open(root).unwrap();
    let id = repo.head_id().unwrap().detach();
    assert_eq!(made.id, id.to_string());
    assert_eq!(
        repo.find_commit(id)
            .unwrap()
            .parent_ids()
            .map(gix::Id::detach)
            .collect::<Vec<_>>(),
        [before]
    );
    assert_eq!(committed(root, "a.txt"), Some(staged));
    assert_eq!(committed(root, "sub/new.txt"), Some(new));
    assert_eq!(committed(root, "b.txt").as_deref(), Some("two\n"));
    assert_eq!(committed(root, "gone.txt"), None);
    assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), index);
    assert!(!root.join(".git/index.lock").exists());
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), later);
    assert_eq!(
        changes(&mut host).await,
        [
            ("a.txt".to_string(), GitArea::Unstaged, GitChange::Modified),
            ("b.txt".to_string(), GitArea::Unstaged, GitChange::Modified),
        ]
    );
}

#[tokio::test]
async fn a_new_repository_commits_what_is_staged() {
    let (dir, store) = store();
    let root = &project(dir.path());
    gix::init(root).unwrap();
    configure(root, "[user]\n\tname = n\n\temail = n@example.invalid\n");
    let mut host = writer(&store, root);
    assert_eq!(
        code(host.handle(commit_staged("m")).await),
        Some(HostErrorCode::Invalid),
        "nothing is staged yet"
    );
    let text = unique("text");
    std::fs::write(root.join("first.txt"), &text).unwrap();
    stage(&mut host, &["first.txt"]).await;
    let reply = host.handle(commit_staged(&unique("first"))).await;
    assert!(matches!(reply, HostReply::Committed(_)), "{reply:?}");
    let repo = gix::open(root).unwrap();
    let made = repo.head_commit().unwrap();
    assert_eq!(made.parent_ids().count(), 0);
    assert_eq!(committed(root, "first.txt"), Some(text));
    assert!(changes(&mut host).await.is_empty());
}

#[tokio::test]
async fn committing_what_is_staged_is_refused_like_any_commit() {
    for setup in ["nothing staged", "no grant", "hook", "locked", "no message"] {
        let (dir, store) = store();
        let root = &project(dir.path());
        committable(root, &[("a.txt", "one\n")]);
        let mut host = writer(&store, root);
        if setup != "nothing staged" {
            std::fs::write(root.join("a.txt"), unique("a")).unwrap();
            stage(&mut host, &["a.txt"]).await;
        }
        let mut message = "m";
        let expected = match setup {
            "no grant" => {
                host = workspace_calls(
                    &store,
                    Some(root.into()),
                    &[Capability::GitRead, Capability::WriteWorkspace],
                );
                HostErrorCode::Denied
            }
            "hook" => {
                std::fs::write(root.join(".git/hooks/pre-commit"), "").unwrap();
                HostErrorCode::Unsupported
            }
            "locked" => {
                std::fs::write(root.join(".git/index.lock"), "").unwrap();
                HostErrorCode::Conflict
            }
            "no message" => {
                message = " \n";
                HostErrorCode::Invalid
            }
            _ => HostErrorCode::Invalid,
        };
        let before = head(root);
        assert_eq!(
            code(host.handle(commit_staged(message)).await),
            Some(expected),
            "{setup}"
        );
        assert_eq!(head(root), before, "{setup}");
    }
}
