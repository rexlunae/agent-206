//! The git staging host call, against repositories built with gitoxide.
use super::git::{repository, unique};
use super::git_commit::{changes, configure, head, project, writer};
use super::*;
use a206_extensions::abi::{GitArea, GitChange};
use std::path::Path;

fn stage(paths: &[&str], unstage: bool) -> HostRequest {
    HostRequest::GitStage {
        paths: paths.iter().map(|path| (*path).into()).collect(),
        unstage,
    }
}
fn index(root: &Path) -> Vec<u8> {
    std::fs::read(root.join(".git/index")).unwrap()
}
/// The blob text the index holds for `path`, if any.
fn staged(root: &Path, path: &str) -> Option<String> {
    let repo = gix::open(root).unwrap();
    let index = repo.open_index().unwrap();
    let entry = index.entry_by_path(path.into())?;
    let blob = repo.find_object(entry.id).unwrap();
    Some(String::from_utf8(blob.detach().data).unwrap())
}

#[tokio::test]
async fn staging_puts_exactly_the_named_files_in_the_index() {
    let (dir, store) = store();
    let root = &project(dir.path());
    repository(
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
    let before = head(root);
    let mut host = writer(&store, root);

    let reply = host
        .handle(stage(&["a.txt", "sub//c.txt", "gone.txt", "a.txt"], false))
        .await;
    assert_eq!(reply, HostReply::Done);
    assert_eq!(staged(root, "a.txt"), Some(a.clone()));
    assert_eq!(staged(root, "sub/c.txt"), Some(c));
    assert_eq!(staged(root, "b.txt").as_deref(), Some("two\n"));
    assert_eq!(staged(root, "gone.txt"), None);
    // Nothing else moved: HEAD, the working tree and the path left out.
    assert_eq!(head(root), before);
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), a);
    assert_eq!(
        changes(&mut host).await,
        [
            ("a.txt".to_string(), GitArea::Staged, GitChange::Modified),
            ("b.txt".to_string(), GitArea::Unstaged, GitChange::Modified),
            ("gone.txt".to_string(), GitArea::Staged, GitChange::Removed),
            ("sub/c.txt".to_string(), GitArea::Staged, GitChange::Added),
        ]
    );
}

#[tokio::test]
async fn unstaging_sets_the_named_paths_back_to_head() {
    let (dir, store) = store();
    let root = &project(dir.path());
    repository(root, &[("a.txt", "one\n"), ("gone.txt", "old\n")]);
    let (a, new) = (unique("a"), unique("new"));
    std::fs::write(root.join("a.txt"), &a).unwrap();
    std::fs::write(root.join("new.txt"), &new).unwrap();
    std::fs::remove_file(root.join("gone.txt")).unwrap();
    let mut host = writer(&store, root);
    let all = ["a.txt", "new.txt", "gone.txt"];
    assert_eq!(host.handle(stage(&all, false)).await, HostReply::Done);
    assert_eq!(host.handle(stage(&all, true)).await, HostReply::Done);
    assert_eq!(staged(root, "a.txt").as_deref(), Some("one\n"));
    assert_eq!(staged(root, "gone.txt").as_deref(), Some("old\n"));
    assert_eq!(staged(root, "new.txt"), None);
    // The changes are back in the working tree only, untouched.
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), a);
    assert_eq!(
        changes(&mut host).await,
        [
            ("a.txt".to_string(), GitArea::Unstaged, GitChange::Modified),
            (
                "gone.txt".to_string(),
                GitArea::Unstaged,
                GitChange::Removed
            ),
            ("new.txt".to_string(), GitArea::Untracked, GitChange::Added),
        ]
    );
}

#[tokio::test]
async fn a_new_repository_stages_and_unstages() {
    let (dir, store) = store();
    let root = &project(dir.path());
    gix::init(root).unwrap();
    let text = unique("text");
    std::fs::write(root.join("first.txt"), &text).unwrap();
    let mut host = writer(&store, root);
    assert_eq!(
        host.handle(stage(&["first.txt"], false)).await,
        HostReply::Done
    );
    assert_eq!(staged(root, "first.txt"), Some(text));
    assert_eq!(
        host.handle(stage(&["first.txt"], true)).await,
        HostReply::Done
    );
    assert_eq!(staged(root, "first.txt"), None);
}

#[tokio::test]
async fn staging_needs_the_git_write_grant() {
    let (dir, store) = store();
    let root = &project(dir.path());
    repository(root, &[("a.txt", "one\n")]);
    std::fs::write(root.join("a.txt"), unique("a")).unwrap();
    let before = index(root);
    let mut reader = workspace_calls(
        &store,
        Some(root.into()),
        &[Capability::GitRead, Capability::WriteWorkspace],
    );
    for unstage in [false, true] {
        assert_eq!(
            code(reader.handle(stage(&["a.txt"], unstage)).await),
            Some(HostErrorCode::Denied)
        );
    }
    assert_eq!(index(root), before);
}

#[tokio::test]
async fn refused_requests_leave_the_index_alone() {
    for (setup, paths, unstage, expected) in [
        ("", vec!["../outside"], false, HostErrorCode::Invalid),
        ("", vec![".git/config"], false, HostErrorCode::Invalid),
        ("", vec![], false, HostErrorCode::Invalid),
        ("", vec!["missing.txt"], false, HostErrorCode::NotFound),
        ("", vec!["missing.txt"], true, HostErrorCode::NotFound),
        // One refused path refuses the others with it.
        (
            "ignored",
            vec!["a.txt", "secret.env"],
            false,
            HostErrorCode::Invalid,
        ),
        ("filter", vec!["a.txt"], false, HostErrorCode::Unsupported),
        (
            "line endings",
            vec!["a.txt"],
            false,
            HostErrorCode::Unsupported,
        ),
        (
            "below a file",
            vec!["a.txt", "b.txt/c"],
            false,
            HostErrorCode::Conflict,
        ),
        ("directory", vec!["dir"], false, HostErrorCode::Unsupported),
        ("directory", vec!["dir"], true, HostErrorCode::Unsupported),
        ("locked", vec!["a.txt"], false, HostErrorCode::Conflict),
    ] {
        let (dir, store) = store();
        let root = &project(dir.path());
        repository(root, &[("a.txt", "one\n"), ("b.txt", "two\n")]);
        std::fs::write(root.join("a.txt"), unique("a")).unwrap();
        match setup {
            "ignored" => {
                std::fs::write(root.join(".gitignore"), "*.env\n").unwrap();
                std::fs::write(root.join("secret.env"), unique("s")).unwrap();
            }
            "filter" => std::fs::write(root.join(".gitattributes"), "*.txt filter=lfs\n").unwrap(),
            "line endings" => {
                std::fs::write(root.join(".gitattributes"), "*.txt text\n").unwrap();
                std::fs::write(root.join("a.txt"), format!("{}\r\n", unique("a"))).unwrap();
            }
            "below a file" => {
                // `b.txt` becomes a directory in the working tree only.
                std::fs::remove_file(root.join("b.txt")).unwrap();
                std::fs::create_dir(root.join("b.txt")).unwrap();
                std::fs::write(root.join("b.txt/c"), unique("c")).unwrap();
            }
            "directory" => {
                std::fs::create_dir(root.join("dir")).unwrap();
                std::fs::write(root.join("dir/x"), unique("x")).unwrap();
                let mut host = writer(&store, root);
                assert_eq!(host.handle(stage(&["dir/x"], false)).await, HostReply::Done);
            }
            "locked" => std::fs::write(root.join(".git/index.lock"), "").unwrap(),
            _ => {}
        }
        let before = index(root);
        let mut host = writer(&store, root);
        assert_eq!(
            code(host.handle(stage(&paths, unstage)).await),
            Some(expected),
            "{setup} {paths:?} {unstage}"
        );
        assert_eq!(index(root), before, "{setup} {paths:?}");
    }
}

#[tokio::test]
async fn staging_never_runs_repository_programs() {
    let (dir, store) = store();
    let root = &project(dir.path());
    repository(root, &[("a.txt", "one\n")]);
    // A clean filter that would run if staging applied it.
    let marker = dir.path().join(unique("ran"));
    configure(
        root,
        &format!(
            "[filter \"x\"]\n\tclean = touch {}\n\trequired = true\n",
            marker.display()
        ),
    );
    std::fs::write(root.join(".gitattributes"), "*.txt filter=x\n").unwrap();
    std::fs::write(root.join("a.txt"), unique("a")).unwrap();
    let mut host = writer(&store, root);
    assert_eq!(
        code(host.handle(stage(&["a.txt"], false)).await),
        Some(HostErrorCode::Unsupported)
    );
    assert!(!marker.exists());
}

#[tokio::test]
async fn a_file_and_a_directory_never_share_a_path_in_the_index() {
    let (dir, store) = store();
    let root = &project(dir.path());
    repository(root, &[("a", "one\n")]);
    let x = unique("x");
    // `a` becomes a directory in the working tree.
    std::fs::remove_file(root.join("a")).unwrap();
    std::fs::create_dir(root.join("a")).unwrap();
    std::fs::write(root.join("a/x"), &x).unwrap();
    let mut host = writer(&store, root);
    // Adding `a/x` while the index keeps the file `a` would hold both, and
    // naming the directory alone would drop `a` without what replaced it.
    let before = index(root);
    assert_eq!(
        code(host.handle(stage(&["a/x"], false)).await),
        Some(HostErrorCode::Conflict)
    );
    assert_eq!(
        code(host.handle(stage(&["a"], false)).await),
        Some(HostErrorCode::Unsupported)
    );
    assert_eq!(index(root), before);
    assert_eq!(
        host.handle(stage(&["a", "a/x"], false)).await,
        HostReply::Done
    );
    assert_eq!(staged(root, "a"), None);
    assert_eq!(staged(root, "a/x"), Some(x.clone()));

    // Unstaging only `a` would put HEAD's file back beside `a/x`.
    let before = index(root);
    assert_eq!(
        code(host.handle(stage(&["a"], true)).await),
        Some(HostErrorCode::Conflict)
    );
    assert_eq!(index(root), before);
    assert_eq!(
        host.handle(stage(&["a", "a/x"], true)).await,
        HostReply::Done
    );
    assert_eq!(staged(root, "a").as_deref(), Some("one\n"));
    assert_eq!(staged(root, "a/x"), None);

    // And back: the directory `a` turns into a file again.
    assert_eq!(
        host.handle(stage(&["a", "a/x"], false)).await,
        HostReply::Done
    );
    std::fs::remove_dir_all(root.join("a")).unwrap();
    let file = unique("file");
    std::fs::write(root.join("a"), &file).unwrap();
    let before = index(root);
    assert_eq!(
        code(host.handle(stage(&["a"], false)).await),
        Some(HostErrorCode::Conflict)
    );
    assert_eq!(index(root), before);
    assert_eq!(
        host.handle(stage(&["a", "a/x"], false)).await,
        HostReply::Done
    );
    assert_eq!(staged(root, "a"), Some(file));
    assert_eq!(staged(root, "a/x"), None);
}

#[tokio::test]
async fn unstaging_does_not_read_the_users_git_configuration() {
    let (dir, store) = store();
    let root = &project(dir.path());
    repository(root, &[("a.txt", "one\n")]);
    std::fs::write(root.join("a.txt"), unique("a")).unwrap();
    let mut host = writer(&store, root);
    assert_eq!(host.handle(stage(&["a.txt"], false)).await, HostReply::Done);
    // Unreadable user configuration refuses staging but not unstaging.
    host.user_git = || Err(failure(HostErrorCode::Io, "unreadable"));
    assert_eq!(
        code(host.handle(stage(&["a.txt"], false)).await),
        Some(HostErrorCode::Io)
    );
    assert_eq!(host.handle(stage(&["a.txt"], true)).await, HostReply::Done);
    assert_eq!(staged(root, "a.txt").as_deref(), Some("one\n"));
}
