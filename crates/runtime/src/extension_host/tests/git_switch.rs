//! The git branch switch host call, against repositories built with
//! gitoxide.
use super::git::{repository, unique};
use super::git_commit::{changes, configure, head, project};
use super::*;
use a206_extensions::abi::{GitArea, GitChange};
use gix::objs::tree::EntryKind;
use std::path::Path;

fn switch(name: &str) -> HostRequest {
    HostRequest::GitSwitchBranch { name: name.into() }
}
fn switcher(store: &Arc<Mutex<Store>>, root: &Path) -> HostCalls {
    workspace_calls(
        store,
        Some(root.into()),
        &[
            Capability::GitRead,
            Capability::GitWrite,
            Capability::WriteWorkspace,
        ],
    )
}
/// A repository holding `files` with a committer for reflogs; the current
/// branch's name.
fn switchable(root: &Path, files: &[(&str, &str)]) -> String {
    repository(root, files);
    configure(root, "[user]\n\tname = n\n\temail = n@example.invalid\n");
    current(root).unwrap()
}
fn current(root: &Path) -> Option<String> {
    gix::open(root)
        .unwrap()
        .head_name()
        .unwrap()
        .map(|name| name.shorten().to_string())
}
/// A new branch whose commit, on top of `HEAD`, holds exactly `files`.
fn branch_with(root: &Path, files: &[(&str, &str, EntryKind)]) -> (String, gix::ObjectId) {
    let repo = gix::open(root).unwrap();
    let mut tree = repo
        .edit_tree(gix::ObjectId::empty_tree(repo.object_hash()))
        .unwrap();
    for (path, text, kind) in files {
        let id = repo.write_blob(text.as_bytes()).unwrap().detach();
        tree.upsert(*path, *kind, id).unwrap();
    }
    let tree = tree.write().unwrap().detach();
    let signature = gix::actor::Signature {
        name: "n".into(),
        email: "n@example.invalid".into(),
        time: gix::date::Time::new(2_000_000, 0),
    };
    let (mut a, mut b) = Default::default();
    let name = unique("branch");
    let commit = repo
        .commit_as(
            signature.to_ref(&mut a),
            signature.to_ref(&mut b),
            format!("refs/heads/{name}"),
            unique("on branch"),
            tree,
            [head(root).unwrap()],
        )
        .unwrap()
        .detach();
    (name, commit)
}
fn text(root: &Path, path: &str) -> Option<String> {
    std::fs::read_to_string(root.join(path)).ok()
}
/// Every regular file and link below `root` outside `.git`, with content.
fn tree_files(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name() == ".git" {
                continue;
            }
            let kind = entry.file_type().unwrap();
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if kind.is_dir() {
                found.push((relative + "/", Vec::new()));
                pending.push(path);
            } else if kind.is_symlink() {
                found.push((
                    relative,
                    std::fs::read_link(&path)
                        .unwrap()
                        .into_os_string()
                        .into_encoded_bytes(),
                ));
            } else {
                found.push((relative, std::fs::read(&path).unwrap()));
            }
        }
    }
    found.sort();
    found
}

#[tokio::test]
async fn switching_changes_the_files_index_and_head() {
    let (dir, store) = store();
    let root = &project(dir.path());
    let (one, same, gone) = (unique("one"), unique("same"), unique("gone"));
    let main = switchable(
        root,
        &[("a.txt", &one), ("keep.txt", &same), ("gone.txt", &gone)],
    );
    let (two, deep, script) = (unique("two"), unique("deep"), unique("#!/bin/sh"));
    let (branch, commit) = branch_with(
        root,
        &[
            ("a.txt", &two, EntryKind::Blob),
            ("keep.txt", &same, EntryKind::Blob),
            ("new/dir/b.txt", &deep, EntryKind::Blob),
            ("run.sh", &script, EntryKind::BlobExecutable),
        ],
    );
    let notes = unique("untracked");
    std::fs::write(root.join("notes.txt"), &notes).unwrap();
    let mut host = switcher(&store, root);

    let reply = host.handle(switch(&branch)).await;
    let HostReply::Branch(switched) = reply else {
        panic!("switch failed: {reply:?}");
    };
    assert!(switched.current);
    assert_eq!(
        (switched.name.as_str(), switched.commit.as_str()),
        (branch.as_str(), commit.to_string().as_str())
    );
    assert_eq!(current(root).as_deref(), Some(branch.as_str()));
    assert_eq!(head(root), Some(commit));
    assert_eq!(text(root, "a.txt"), Some(two));
    assert_eq!(text(root, "keep.txt"), Some(same.clone()));
    assert_eq!(text(root, "new/dir/b.txt"), Some(deep));
    assert_eq!(text(root, "run.sh"), Some(script));
    assert_eq!(text(root, "gone.txt"), None);
    // Untracked files stay where they are.
    assert_eq!(text(root, "notes.txt"), Some(notes.clone()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &str| {
            std::fs::metadata(root.join(path))
                .unwrap()
                .permissions()
                .mode()
        };
        assert_ne!(mode("run.sh") & 0o100, 0);
        assert_eq!(mode("a.txt") & 0o111, 0);
    }
    // The index matches the branch: only the untracked file shows.
    let untracked = ("notes.txt".to_owned(), GitArea::Untracked, GitChange::Added);
    assert_eq!(changes(&mut host).await, std::slice::from_ref(&untracked));

    // Back again: the branch's files and its emptied folders are gone.
    let reply = host.handle(switch(&main)).await;
    assert!(matches!(reply, HostReply::Branch(_)), "{reply:?}");
    assert_eq!(current(root).as_deref(), Some(main.as_str()));
    assert_eq!(text(root, "a.txt"), Some(one));
    assert_eq!(text(root, "gone.txt"), Some(gone));
    assert_eq!(text(root, "keep.txt"), Some(same));
    assert!(!root.join("new").exists());
    assert!(!root.join("run.sh").exists());
    assert_eq!(changes(&mut host).await, [untracked]);
    let log = std::fs::read_to_string(root.join(".git/logs/HEAD")).unwrap();
    let [.., there, back] = &log.lines().collect::<Vec<_>>()[..] else {
        panic!("{log}");
    };
    assert!(
        there.ends_with(&format!("\tcheckout: moving from {main} to {branch}")),
        "{log}"
    );
    assert!(back.starts_with(&format!("{commit} ")), "{log}");
    assert!(
        back.ends_with(&format!("\tcheckout: moving from {branch} to {main}")),
        "{log}"
    );
}

#[tokio::test]
async fn files_and_folders_trade_places() {
    let (dir, store) = store();
    let root = &project(dir.path());
    let (file, below) = (unique("file"), unique("below"));
    let main = switchable(root, &[("x", &file)]);
    let (branch, _) = branch_with(root, &[("x/y", &below, EntryKind::Blob)]);
    let mut host = switcher(&store, root);
    let reply = host.handle(switch(&branch)).await;
    assert!(matches!(reply, HostReply::Branch(_)), "{reply:?}");
    assert_eq!(text(root, "x/y"), Some(below));
    let reply = host.handle(switch(&main)).await;
    assert!(matches!(reply, HostReply::Branch(_)), "{reply:?}");
    assert_eq!(text(root, "x"), Some(file));
    assert!(changes(&mut host).await.is_empty());
}

#[tokio::test]
async fn changes_and_files_in_the_way_refuse_a_switch() {
    for setup in [
        "modified",
        "staged",
        "untracked file in the way",
        "untracked file in a folder in the way",
        "ignored file in the way",
        "untracked file below a file in the way",
    ] {
        let (dir, store) = store();
        let root = &project(dir.path());
        let main = switchable(root, &[("a.txt", "one\n"), (".gitignore", "*.log\n")]);
        let (branch, _) = branch_with(
            root,
            &[
                ("a.txt", "two\n", EntryKind::Blob),
                ("new.txt", "branch\n", EntryKind::Blob),
                ("out.log", "branch\n", EntryKind::Blob),
                ("folder", "branch\n", EntryKind::Blob),
                ("file/inside", "branch\n", EntryKind::Blob),
            ],
        );
        let mut host = switcher(&store, root);
        match setup {
            "modified" => std::fs::write(root.join("a.txt"), "edited\n").unwrap(),
            "staged" => {
                std::fs::write(root.join("a.txt"), "staged\n").unwrap();
                let reply = host
                    .handle(HostRequest::GitStage {
                        paths: vec!["a.txt".into()],
                        unstage: false,
                    })
                    .await;
                assert!(matches!(reply, HostReply::Done), "{reply:?}");
                std::fs::write(root.join("a.txt"), "one\n").unwrap();
            }
            "untracked file in the way" => std::fs::write(root.join("new.txt"), "mine\n").unwrap(),
            "untracked file in a folder in the way" => {
                std::fs::create_dir(root.join("folder")).unwrap();
                std::fs::write(root.join("folder/mine"), "mine\n").unwrap();
            }
            "ignored file in the way" => std::fs::write(root.join("out.log"), "mine\n").unwrap(),
            _ => std::fs::write(root.join("file"), "mine\n").unwrap(),
        }
        let before = tree_files(root);
        assert_eq!(
            code(host.handle(switch(&branch)).await),
            Some(HostErrorCode::Conflict),
            "{setup}"
        );
        assert_eq!(current(root).as_deref(), Some(main.as_str()), "{setup}");
        assert_eq!(tree_files(root), before, "{setup}");
        assert!(!root.join(".git/index.lock").exists(), "{setup}");
    }
}

#[tokio::test]
async fn protections_git_would_apply_refuse_a_switch() {
    for setup in [
        "hook",
        "autocrlf",
        "filter",
        "eol",
        "link",
        "no identity",
        "reflog",
        "missing",
        "invalid",
    ] {
        let (dir, store) = store();
        let root = &project(dir.path());
        let main = if setup == "no identity" {
            repository(root, &[("a.txt", "one\n")]);
            current(root).unwrap()
        } else {
            switchable(root, &[("a.txt", "one\n")])
        };
        let mut files = vec![("a.txt", "two\n", EntryKind::Blob)];
        let (name, expected) = match setup {
            "hook" => {
                std::fs::write(root.join(".git/hooks/post-checkout"), "").unwrap();
                (None, HostErrorCode::Unsupported)
            }
            "autocrlf" => {
                configure(root, "[core]\n\tautocrlf = true\n");
                (None, HostErrorCode::Unsupported)
            }
            "filter" => {
                files.push((".gitattributes", "*.txt filter=lfs\n", EntryKind::Blob));
                (None, HostErrorCode::Unsupported)
            }
            "eol" => {
                files.push((".gitattributes", "*.txt text eol=crlf\n", EntryKind::Blob));
                (None, HostErrorCode::Unsupported)
            }
            "link" => {
                files.push(("link", "a.txt", EntryKind::Link));
                (None, HostErrorCode::Unsupported)
            }
            "no identity" => (None, HostErrorCode::Unsupported),
            "reflog" => {
                let log = root.join(".git/logs/HEAD");
                std::fs::remove_file(&log).unwrap();
                std::fs::create_dir(&log).unwrap();
                (None, HostErrorCode::Io)
            }
            "missing" => (Some(unique("missing")), HostErrorCode::NotFound),
            _ => (Some("-x".to_owned()), HostErrorCode::Invalid),
        };
        let (branch, _) = branch_with(root, &files);
        let mut host = switcher(&store, root);
        let before = tree_files(root);
        let name = name.unwrap_or(branch);
        assert_eq!(
            code(host.handle(switch(&name)).await),
            Some(expected),
            "{setup}"
        );
        assert_eq!(current(root).as_deref(), Some(main.as_str()), "{setup}");
        assert_eq!(tree_files(root), before, "{setup}");
    }
}

#[tokio::test]
async fn switching_needs_both_grants() {
    let (dir, store) = store();
    let root = &project(dir.path());
    let main = switchable(root, &[("a.txt", "one\n")]);
    let (branch, _) = branch_with(root, &[("a.txt", "two\n", EntryKind::Blob)]);
    for granted in [
        &[Capability::GitRead, Capability::GitWrite][..],
        &[Capability::GitRead, Capability::WriteWorkspace],
    ] {
        let mut host = workspace_calls(&store, Some(root.into()), granted);
        assert_eq!(
            code(host.handle(switch(&branch)).await),
            Some(HostErrorCode::Denied),
            "{granted:?}"
        );
    }
    assert_eq!(current(root).as_deref(), Some(main.as_str()));
    assert_eq!(text(root, "a.txt").as_deref(), Some("one\n"));
    // Switching to the current branch changes nothing.
    let mut host = switcher(&store, root);
    let reply = host.handle(switch(&main)).await;
    let HostReply::Branch(made) = reply else {
        panic!("switch failed: {reply:?}");
    };
    assert!(made.current);
    assert_eq!(current(root).as_deref(), Some(main.as_str()));
}

#[tokio::test]
async fn an_empty_folder_keeps_a_file_from_replacing_its_folder() {
    let (dir, store) = store();
    let root = &project(dir.path());
    switchable(root, &[("a.txt", "one\n")]);
    let (folder, _) = branch_with(root, &[("dir/t.txt", "tracked\n", EntryKind::Blob)]);
    let mut host = switcher(&store, root);
    let reply = host.handle(switch(&folder)).await;
    assert!(matches!(reply, HostReply::Branch(_)), "{reply:?}");
    let (file, _) = branch_with(root, &[("dir", "a file\n", EntryKind::Blob)]);
    std::fs::create_dir(root.join("dir/empty")).unwrap();
    let before = tree_files(root);
    assert_eq!(
        code(host.handle(switch(&file)).await),
        Some(HostErrorCode::Conflict)
    );
    assert_eq!(tree_files(root), before);
    assert_eq!(current(root).as_deref(), Some(folder.as_str()));
    // Without it, the folder gives way to the file.
    std::fs::remove_dir(root.join("dir/empty")).unwrap();
    let reply = host.handle(switch(&file)).await;
    assert!(matches!(reply, HostReply::Branch(_)), "{reply:?}");
    assert_eq!(text(root, "dir").as_deref(), Some("a file\n"));
}
